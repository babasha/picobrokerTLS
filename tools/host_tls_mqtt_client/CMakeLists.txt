cmake_minimum_required(VERSION 3.20)

project(tls_mqtt_client C)

if(NOT DEFINED MBEDTLS_ROOT)
    message(FATAL_ERROR "MBEDTLS_ROOT is required")
endif()

if(NOT DEFINED MBEDTLS_BUILD_ROOT)
    message(FATAL_ERROR "MBEDTLS_BUILD_ROOT is required")
endif()

add_executable(tls_mqtt_client tls_mqtt_client.c)

set_target_properties(tls_mqtt_client PROPERTIES C_STANDARD 99 C_STANDARD_REQUIRED YES)

target_include_directories(
    tls_mqtt_client
    PRIVATE
        "${MBEDTLS_ROOT}/include"
)

target_link_libraries(
    tls_mqtt_client
    PRIVATE
        "${MBEDTLS_BUILD_ROOT}/library/$<CONFIG>/mbedtls.lib"
        "${MBEDTLS_BUILD_ROOT}/library/$<CONFIG>/mbedx509.lib"
        "${MBEDTLS_BUILD_ROOT}/library/$<CONFIG>/mbedcrypto.lib"
        "${MBEDTLS_BUILD_ROOT}/3rdparty/everest/$<CONFIG>/everest.lib"
        "${MBEDTLS_BUILD_ROOT}/3rdparty/p256-m/$<CONFIG>/p256m.lib"
        ws2_32
        bcrypt
        advapi32
)
