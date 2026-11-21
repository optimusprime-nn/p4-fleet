idf_component_register(
    SRCS "fleet_agent.c"
    INCLUDE_DIRS "include"
    REQUIRES esp_http_server
    PRIV_REQUIRES
        esp_app_format app_update esp_netif esp_event esp_timer
        esp_hw_support esp_system lwip
)
