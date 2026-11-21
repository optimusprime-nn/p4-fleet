#include "fleet_agent.h"

#include <stdio.h>
#include <string.h>
#include <time.h>
#include "cJSON.h"
#include "esp_app_desc.h"
#include "esp_chip_info.h"
#include "esp_event.h"
#include "esp_heap_caps.h"
#include "esp_log.h"
#include "esp_mac.h"
#include "esp_netif.h"
#include "esp_ota_ops.h"
#include "esp_system.h"
#include "esp_timer.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "sdkconfig.h"
#if CONFIG_FLEET_AGENT_MDNS
#include "mdns.h"
#endif

#define FLEET_SCHEMA 1

static const char *TAG = "fleet";

static fleet_agent_config_t s_cfg;
static char s_id[13];
static char s_ip[16];
static bool s_started;
#if CONFIG_FLEET_AGENT_MDNS
static bool s_mdns_up;
#endif

const char *fleet_agent_device_id(void)
{
    if (!s_id[0]) {
        uint8_t mac[6] = {0};
        esp_efuse_mac_get_default(mac);
        snprintf(s_id, sizeof(s_id), "%02x%02x%02x%02x%02x%02x",
                 mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]);
    }
    return s_id;
}

static const char *token(void)
{
    if (s_cfg.token && s_cfg.token[0]) return s_cfg.token;
    return CONFIG_FLEET_AGENT_TOKEN;
}

static bool writes_enabled(void)
{
    return token()[0] != 0;
}

static const char *reset_reason_str(esp_reset_reason_t r)
{
    switch (r) {
    case ESP_RST_POWERON:   return "power_on";
    case ESP_RST_EXT:       return "external";
    case ESP_RST_SW:        return "software";
    case ESP_RST_PANIC:     return "panic";
    case ESP_RST_INT_WDT:   return "int_wdt";
    case ESP_RST_TASK_WDT:  return "task_wdt";
    case ESP_RST_WDT:       return "wdt";
    case ESP_RST_DEEPSLEEP: return "deep_sleep";
    case ESP_RST_BROWNOUT:  return "brownout";
    case ESP_RST_SDIO:      return "sdio";
    default:                return "unknown";
    }
}

/* ------------------------------------------------------------------------- */
/* HTTP                                                                      */
/* ------------------------------------------------------------------------- */

static esp_err_t send_text(httpd_req_t *req, const char *status, const char *msg)
{
    httpd_resp_set_status(req, status);
    httpd_resp_set_type(req, "text/plain");
    return httpd_resp_sendstr(req, msg);
}

/* Constant-time comparison so the token can't be guessed byte by byte */
static bool token_equal(const char *a, const char *b)
{
    size_t la = strlen(a), lb = strlen(b);
    unsigned char diff = (unsigned char)(la != lb);
    for (size_t i = 0; i < la; i++) diff |= (unsigned char)(a[i] ^ b[i % (lb ? lb : 1)]);
    return diff == 0;
}

/* Returns ESP_OK if the request may continue, otherwise the response is sent */
static esp_err_t check_auth(httpd_req_t *req)
{
    if (!writes_enabled()) {
        send_text(req, "403 Forbidden", "fleet writes disabled: no fleet token configured on this device");
        return ESP_FAIL;
    }
    char hdr[160] = {0};
    if (httpd_req_get_hdr_value_str(req, "Authorization", hdr, sizeof(hdr)) != ESP_OK ||
        strncmp(hdr, "Bearer ", 7) != 0 || !token_equal(hdr + 7, token())) {
        send_text(req, "401 Unauthorized", "missing or wrong fleet token");
        return ESP_FAIL;
    }
    return ESP_OK;
}

static esp_err_t info_get(httpd_req_t *req)
{
    const esp_app_desc_t *d = esp_app_get_description();
    const esp_partition_t *run = esp_ota_get_running_partition();
    esp_ota_img_states_t ost = ESP_OTA_IMG_UNDEFINED;
    bool pending = run && esp_ota_get_state_partition(run, &ost) == ESP_OK && ost == ESP_OTA_IMG_PENDING_VERIFY;
    char elf[65] = {0};
    esp_app_get_elf_sha256(elf, sizeof(elf));
    esp_chip_info_t chip;
    esp_chip_info(&chip);
    char build[40];
    snprintf(build, sizeof(build), "%s %s", d->date, d->time);
    const char *host = CONFIG_LWIP_LOCAL_HOSTNAME;
    esp_netif_t *sta = esp_netif_get_handle_from_ifkey("WIFI_STA_DEF");
    if (sta) esp_netif_get_hostname(sta, &host);

    cJSON *root = cJSON_CreateObject();
    if (!root) return send_text(req, "500 Internal Server Error", "out of memory");
    cJSON_AddNumberToObject(root, "schema", FLEET_SCHEMA);
    cJSON_AddStringToObject(root, "id", fleet_agent_device_id());
    cJSON_AddStringToObject(root, "hostname", host ? host : "");
    cJSON_AddStringToObject(root, "label", s_cfg.label ? s_cfg.label : d->project_name);
    cJSON_AddStringToObject(root, "project", d->project_name);
    cJSON_AddStringToObject(root, "version", d->version);
    cJSON_AddStringToObject(root, "idf", d->idf_ver);
    cJSON_AddStringToObject(root, "build", build);
    cJSON_AddStringToObject(root, "elf_sha256", elf);
    cJSON_AddStringToObject(root, "chip", CONFIG_IDF_TARGET);
    cJSON_AddNumberToObject(root, "chip_rev", chip.revision);
    cJSON_AddStringToObject(root, "partition", run ? run->label : "?");
    cJSON_AddBoolToObject(root, "pending_verify", pending);
    cJSON_AddNumberToObject(root, "uptime_s", (double)(esp_timer_get_time() / 1000000));
    cJSON_AddStringToObject(root, "reset_reason", reset_reason_str(esp_reset_reason()));
    time_t now = time(NULL);
    cJSON_AddNumberToObject(root, "time", now > 1700000000 ? (double)now : 0);
    cJSON_AddStringToObject(root, "ip", s_ip);
    if (s_cfg.rssi) cJSON_AddNumberToObject(root, "rssi", s_cfg.rssi());

    cJSON *heap = cJSON_AddObjectToObject(root, "heap");
    cJSON_AddNumberToObject(heap, "int", heap_caps_get_free_size(MALLOC_CAP_INTERNAL));
    cJSON_AddNumberToObject(heap, "int_min", heap_caps_get_minimum_free_size(MALLOC_CAP_INTERNAL));
    cJSON_AddNumberToObject(heap, "psram", heap_caps_get_free_size(MALLOC_CAP_SPIRAM));
    cJSON_AddNumberToObject(heap, "psram_min", heap_caps_get_minimum_free_size(MALLOC_CAP_SPIRAM));

    cJSON *caps = cJSON_AddObjectToObject(root, "caps");
    bool w = writes_enabled();
    cJSON_AddBoolToObject(caps, "ota", w && s_cfg.ota_receive);
    cJSON_AddBoolToObject(caps, "reboot", w);
    cJSON_AddBoolToObject(caps, "revert", w && s_cfg.revert);
    cJSON_AddBoolToObject(caps, "identify", w && s_cfg.identify);

    cJSON *status = cJSON_AddObjectToObject(root, "status");
    if (s_cfg.fill_status) s_cfg.fill_status(status);
    cJSON *app = cJSON_AddObjectToObject(root, "app");
    if (s_cfg.fill_app) s_cfg.fill_app(app);

    char *txt = cJSON_PrintUnformatted(root);
    cJSON_Delete(root);
    if (!txt) return send_text(req, "500 Internal Server Error", "out of memory");
    httpd_resp_set_type(req, "application/json");
    httpd_resp_set_hdr(req, "Cache-Control", "no-store");
    esp_err_t err = httpd_resp_sendstr(req, txt);
    cJSON_free(txt);
    return err;
}

static esp_err_t ota_post(httpd_req_t *req)
{
    if (check_auth(req) != ESP_OK) return ESP_OK;
    if (!s_cfg.ota_receive) return send_text(req, "501 Not Implemented", "this app does not support fleet OTA");
    ESP_LOGW(TAG, "firmware update pushed by the fleet server (%u bytes)", (unsigned)req->content_len);
    return s_cfg.ota_receive(req);
}

static void reboot_task(void *arg)
{
    vTaskDelay(pdMS_TO_TICKS(500));
    esp_restart();
}

static esp_err_t reboot_post(httpd_req_t *req)
{
    if (check_auth(req) != ESP_OK) return ESP_OK;
    ESP_LOGW(TAG, "reboot requested by the fleet server");
    send_text(req, "202 Accepted", "rebooting");
    xTaskCreate(reboot_task, "fleet_reboot", 2048, NULL, 5, NULL);
    return ESP_OK;
}

static void revert_task(void *arg)
{
    vTaskDelay(pdMS_TO_TICKS(500));
    if (!s_cfg.revert()) ESP_LOGE(TAG, "revert failed: no valid previous firmware");
    vTaskDelete(NULL);
}

static esp_err_t revert_post(httpd_req_t *req)
{
    if (check_auth(req) != ESP_OK) return ESP_OK;
    if (!s_cfg.revert) return send_text(req, "501 Not Implemented", "this app does not support revert");
    ESP_LOGW(TAG, "revert to the previous firmware requested by the fleet server");
    send_text(req, "202 Accepted", "reverting to the previous firmware");
    xTaskCreate(revert_task, "fleet_revert", 4096, NULL, 5, NULL);
    return ESP_OK;
}

static esp_err_t identify_post(httpd_req_t *req)
{
    if (check_auth(req) != ESP_OK) return ESP_OK;
    if (!s_cfg.identify) return send_text(req, "501 Not Implemented", "this app does not support identify");
    s_cfg.identify();
    return send_text(req, "200 OK", "identifying");
}

/* ------------------------------------------------------------------------- */
/* mDNS                                                                      */
/* ------------------------------------------------------------------------- */

#if CONFIG_FLEET_AGENT_MDNS
static void mdns_setup(void)
{
    if (mdns_init() != ESP_OK) {
        ESP_LOGE(TAG, "mDNS init failed");
        return;
    }
    const esp_app_desc_t *d = esp_app_get_description();
    const char *host = CONFIG_LWIP_LOCAL_HOSTNAME;
    esp_netif_t *sta = esp_netif_get_handle_from_ifkey("WIFI_STA_DEF");
    if (sta) esp_netif_get_hostname(sta, &host);
    mdns_hostname_set(host && host[0] ? host : "p4-device");

    const char *label = s_cfg.label ? s_cfg.label : d->project_name;
    char instance[64];
    snprintf(instance, sizeof(instance), "%s %s", label, fleet_agent_device_id() + 6);
    mdns_instance_name_set(instance);

    mdns_txt_item_t txt[] = {
        {"id", fleet_agent_device_id()},
        {"project", d->project_name},
        {"ver", d->version},
        {"label", label},
    };
    esp_err_t err = mdns_service_add(instance, "_p4fleet", "_tcp", s_cfg.http_port, txt,
                                     sizeof(txt) / sizeof(txt[0]));
    if (err != ESP_OK) ESP_LOGE(TAG, "mDNS service add failed: %s", esp_err_to_name(err));
    else ESP_LOGI(TAG, "announcing _p4fleet._tcp as \"%s\"", instance);
    s_mdns_up = true;
}

/* With esp_wifi_remote the STA netif may not be one mDNS manages on its own,
 * so register it explicitly. "Already registered" is fine. */
static void mdns_attach(esp_netif_t *netif)
{
    if (!s_mdns_up || !netif) return;
    esp_err_t err = mdns_register_netif(netif);
    if (err != ESP_OK && err != ESP_ERR_INVALID_STATE) {
        ESP_LOGW(TAG, "mDNS register netif: %s", esp_err_to_name(err));
    }
    mdns_netif_action(netif, MDNS_EVENT_ENABLE_IP4 | MDNS_EVENT_ANNOUNCE_IP4);
}
#endif

static void on_got_ip(void *arg, esp_event_base_t base, int32_t id, void *data)
{
    const ip_event_got_ip_t *e = (const ip_event_got_ip_t *)data;
    snprintf(s_ip, sizeof(s_ip), IPSTR, IP2STR(&e->ip_info.ip));
#if CONFIG_FLEET_AGENT_MDNS
    mdns_attach(e->esp_netif);
#endif
}

/* ------------------------------------------------------------------------- */

esp_err_t fleet_agent_start(const fleet_agent_config_t *cfg)
{
    if (s_started) return ESP_ERR_INVALID_STATE;
    if (!cfg || !cfg->register_uri) return ESP_ERR_INVALID_ARG;
    s_cfg = *cfg;
    if (!s_cfg.http_port) s_cfg.http_port = 80;
    fleet_agent_device_id();

    const httpd_uri_t uris[] = {
        {.uri = "/fleet/info", .method = HTTP_GET, .handler = info_get},
        {.uri = "/fleet/ota", .method = HTTP_POST, .handler = ota_post},
        {.uri = "/fleet/reboot", .method = HTTP_POST, .handler = reboot_post},
        {.uri = "/fleet/revert", .method = HTTP_POST, .handler = revert_post},
        {.uri = "/fleet/identify", .method = HTTP_POST, .handler = identify_post},
    };
    for (size_t i = 0; i < sizeof(uris) / sizeof(uris[0]); i++) s_cfg.register_uri(&uris[i]);

    esp_event_handler_register(IP_EVENT, IP_EVENT_STA_GOT_IP, on_got_ip, NULL);
#if CONFIG_FLEET_AGENT_MDNS
    mdns_setup();
#endif
    /* Wi-Fi may already be up */
    esp_netif_t *sta = esp_netif_get_handle_from_ifkey("WIFI_STA_DEF");
    esp_netif_ip_info_t ip;
    if (sta && esp_netif_get_ip_info(sta, &ip) == ESP_OK && ip.ip.addr != 0) {
        snprintf(s_ip, sizeof(s_ip), IPSTR, IP2STR(&ip.ip));
#if CONFIG_FLEET_AGENT_MDNS
        mdns_attach(sta);
#endif
    }

    s_started = true;
    ESP_LOGI(TAG, "fleet agent up: id %s, writes %s", s_id, writes_enabled() ? "enabled" : "disabled (no token)");
    return ESP_OK;
}
