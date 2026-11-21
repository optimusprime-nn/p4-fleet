#pragma once

/*
 * P4 Fleet agent - lets the p4-fleet server find, monitor and update this device.
 *
 * Adds to the app's existing HTTP server (port 80):
 *
 *   GET  /fleet/info       device, firmware, health and app status as JSON (no auth)
 *   POST /fleet/ota        firmware image as the raw body           (Bearer token)
 *   POST /fleet/reboot                                               (Bearer token)
 *   POST /fleet/revert     boot the previous firmware slot           (Bearer token)
 *   POST /fleet/identify   make the device show itself (blink...)    (Bearer token)
 *
 * and announces itself over mDNS as  _p4fleet._tcp  with TXT records
 * id, project, ver and label.
 *
 * The write endpoints need the fleet token (CONFIG_FLEET_AGENT_TOKEN or
 * fleet_agent_config_t.token). With no token they are disabled and the device
 * is monitor-only.
 *
 * The agent knows nothing about the app: OTA, revert, identify and the extra
 * status come from callbacks, so the same component works in every P4 app.
 */

#include <stdbool.h>
#include <stdint.h>
#include "esp_err.h"
#include "esp_http_server.h"

#ifdef __cplusplus
extern "C" {
#endif

struct cJSON;

typedef struct {
    /* Human-readable name of the app, e.g. "ONO display". Shown in the fleet UI. */
    const char *label;
    /* Fleet token for the write endpoints. NULL or "" = CONFIG_FLEET_AGENT_TOKEN. */
    const char *token;
    /* Port the HTTP server listens on (announced over mDNS). 0 = 80. */
    uint16_t http_port;

    /* Registers one URI on the app's HTTP server (required). */
    void (*register_uri)(const httpd_uri_t *uri);

    /* Receive a firmware image from the request body, verify it, make it
     * bootable, send the HTTP response and schedule the reboot.
     * NULL = OTA through the fleet is disabled. */
    esp_err_t (*ota_receive)(httpd_req_t *req);
    /* Boot the image in the other OTA slot (does not return on success).
     * Called from a helper task after the response was sent. NULL = disabled. */
    bool (*revert)(void);
    /* Make the device show itself for a few seconds. NULL = disabled. */
    void (*identify)(void);
    /* Wi-Fi signal strength in dBm (0 = unknown). NULL = not reported. */
    int (*rssi)(void);

    /* Add generic status fields to info["status"], e.g. prev_version,
     * can_revert, last_crash, c6_version. Called in the HTTP server task. */
    void (*fill_status)(struct cJSON *status);
    /* Add app-specific values to info["app"]. Numbers and booleans here are
     * also exported by the server as Prometheus metrics. */
    void (*fill_app)(struct cJSON *app);
} fleet_agent_config_t;

/* Call after the HTTP server is running. The config is copied, but the strings
 * it points to must stay valid. */
esp_err_t fleet_agent_start(const fleet_agent_config_t *cfg);

/* Device id announced to the server: the factory MAC as 12 hex digits. */
const char *fleet_agent_device_id(void);

#ifdef __cplusplus
}
#endif
