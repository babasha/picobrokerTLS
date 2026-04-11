#include "mbedtls/build_info.h"

#include "mbedtls/ctr_drbg.h"
#include "mbedtls/entropy.h"
#include "mbedtls/error.h"
#include "mbedtls/net_sockets.h"
#include "mbedtls/ssl.h"

#if defined(MBEDTLS_USE_PSA_CRYPTO)
#include "psa/crypto.h"
#endif

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#if defined(_WIN32)
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#endif

#define DEFAULT_SERVER_ADDR "192.168.0.7"
#define DEFAULT_SERVER_PORT "8883"
#define DEFAULT_PSK_IDENTITY "gatomqtt-psk"
#define DEFAULT_PSK_HEX "9A3FC152D86B17A4E3297C10B54D8E6213F7A93C58DE71248BC64A9105EF367D"
#define DEFAULT_CLIENT_ID "tls-mqtt-client"
#define DEFAULT_USERNAME "house"
#define DEFAULT_PASSWORD "secret"
#define DEFAULT_KEEP_ALIVE 30
#define DEFAULT_HOLD_MS 3000
#define DEFAULT_READ_TIMEOUT_MS 5000

struct options {
    const char *server_addr;
    const char *server_port;
    const char *psk_identity;
    const char *psk_hex;
    const char *client_id;
    const char *username;
    const char *password;
    unsigned keep_alive;
    unsigned hold_ms;
    unsigned read_timeout_ms;
};

static void print_usage(const char *argv0)
{
    printf("Usage: %s [server_addr=IP] [server_port=PORT] [client_id=ID] [hold_ms=N]\n", argv0);
    printf("           [read_timeout_ms=N] [username=USER] [password=PASS]\n");
    printf("           [psk_identity=ID] [psk=HEX] [keep_alive=N]\n");
}

static int starts_with(const char *value, const char *prefix)
{
    size_t prefix_len = strlen(prefix);
    return strncmp(value, prefix, prefix_len) == 0;
}

static int parse_u32(const char *text, unsigned *out)
{
    char *end = NULL;
    unsigned long value = strtoul(text, &end, 10);
    if (text[0] == '\0' || end == NULL || *end != '\0' || value > 0xffffffffUL) {
        return -1;
    }
    *out = (unsigned) value;
    return 0;
}

static int hex_nibble(char c)
{
    if (c >= '0' && c <= '9') {
        return c - '0';
    }
    if (c >= 'a' && c <= 'f') {
        return 10 + (c - 'a');
    }
    if (c >= 'A' && c <= 'F') {
        return 10 + (c - 'A');
    }
    return -1;
}

static int decode_hex(const char *hex, unsigned char *out, size_t out_size, size_t *out_len)
{
    size_t hex_len = strlen(hex);
    size_t i;

    if ((hex_len % 2u) != 0u) {
        return -1;
    }

    if ((hex_len / 2u) > out_size) {
        return -1;
    }

    for (i = 0; i < hex_len; i += 2u) {
        int hi = hex_nibble(hex[i]);
        int lo = hex_nibble(hex[i + 1u]);
        if (hi < 0 || lo < 0) {
            return -1;
        }
        out[i / 2u] = (unsigned char) ((hi << 4) | lo);
    }

    *out_len = hex_len / 2u;
    return 0;
}

static void write_u16(unsigned char *dst, uint16_t value)
{
    dst[0] = (unsigned char) ((value >> 8) & 0xffu);
    dst[1] = (unsigned char) (value & 0xffu);
}

static int append_utf8(unsigned char *buf, size_t buf_size, size_t *offset, const char *value)
{
    size_t len = strlen(value);
    if (len > 0xffffu || (*offset + 2u + len) > buf_size) {
        return -1;
    }

    write_u16(buf + *offset, (uint16_t) len);
    *offset += 2u;
    memcpy(buf + *offset, value, len);
    *offset += len;
    return 0;
}

static int build_connect_packet(const struct options *opt, unsigned char *buf, size_t buf_size, size_t *packet_len)
{
    size_t payload_offset = 2u;
    unsigned char connect_flags = 0x02u;

    if (buf_size < 2u) {
        return -1;
    }

    if (append_utf8(buf, buf_size, &payload_offset, "MQTT") != 0) {
        return -1;
    }

    if ((payload_offset + 4u) > buf_size) {
        return -1;
    }

    if (opt->username[0] != '\0') {
        connect_flags |= 0x80u;
    }
    if (opt->password[0] != '\0') {
        connect_flags |= 0x40u;
    }

    buf[payload_offset++] = 0x04u;
    buf[payload_offset++] = connect_flags;
    write_u16(buf + payload_offset, (uint16_t) opt->keep_alive);
    payload_offset += 2u;

    if (append_utf8(buf, buf_size, &payload_offset, opt->client_id) != 0) {
        return -1;
    }
    if (opt->username[0] != '\0' && append_utf8(buf, buf_size, &payload_offset, opt->username) != 0) {
        return -1;
    }
    if (opt->password[0] != '\0' && append_utf8(buf, buf_size, &payload_offset, opt->password) != 0) {
        return -1;
    }

    if ((payload_offset - 2u) > 127u) {
        return -1;
    }

    buf[0] = 0x10u;
    buf[1] = (unsigned char) (payload_offset - 2u);
    *packet_len = payload_offset;
    return 0;
}

static void print_mbedtls_error(const char *label, int ret)
{
    char errbuf[128];
    mbedtls_strerror(ret, errbuf, sizeof(errbuf));
    fprintf(stderr, "%s: -0x%04x (%s)\n", label, (unsigned) -ret, errbuf);
}

static int ssl_write_all(mbedtls_ssl_context *ssl, const unsigned char *buf, size_t len)
{
    size_t offset = 0u;

    while (offset < len) {
        int ret = mbedtls_ssl_write(ssl, buf + offset, len - offset);
        if (ret > 0) {
            offset += (size_t) ret;
            continue;
        }
        if (ret == MBEDTLS_ERR_SSL_WANT_READ || ret == MBEDTLS_ERR_SSL_WANT_WRITE) {
            continue;
        }
        return ret;
    }

    return 0;
}

static int ssl_read_exact(mbedtls_ssl_context *ssl, unsigned char *buf, size_t len)
{
    size_t offset = 0u;

    while (offset < len) {
        int ret = mbedtls_ssl_read(ssl, buf + offset, len - offset);
        if (ret > 0) {
            offset += (size_t) ret;
            continue;
        }
        if (ret == MBEDTLS_ERR_SSL_WANT_READ || ret == MBEDTLS_ERR_SSL_WANT_WRITE) {
            continue;
        }
        return ret == 0 ? MBEDTLS_ERR_SSL_CONN_EOF : ret;
    }

    return 0;
}

static void sleep_ms(unsigned milliseconds)
{
#if defined(_WIN32)
    Sleep(milliseconds);
#else
    (void) milliseconds;
#endif
}

static int parse_args(int argc, char **argv, struct options *opt)
{
    int i;

    *opt = (struct options) {
        DEFAULT_SERVER_ADDR,
        DEFAULT_SERVER_PORT,
        DEFAULT_PSK_IDENTITY,
        DEFAULT_PSK_HEX,
        DEFAULT_CLIENT_ID,
        DEFAULT_USERNAME,
        DEFAULT_PASSWORD,
        DEFAULT_KEEP_ALIVE,
        DEFAULT_HOLD_MS,
        DEFAULT_READ_TIMEOUT_MS
    };

    for (i = 1; i < argc; ++i) {
        const char *arg = argv[i];

        if (strcmp(arg, "--help") == 0 || strcmp(arg, "-h") == 0) {
            print_usage(argv[0]);
            return 1;
        }
        if (starts_with(arg, "server_addr=")) {
            opt->server_addr = arg + strlen("server_addr=");
        } else if (starts_with(arg, "server_port=")) {
            opt->server_port = arg + strlen("server_port=");
        } else if (starts_with(arg, "psk_identity=")) {
            opt->psk_identity = arg + strlen("psk_identity=");
        } else if (starts_with(arg, "psk=")) {
            opt->psk_hex = arg + strlen("psk=");
        } else if (starts_with(arg, "client_id=")) {
            opt->client_id = arg + strlen("client_id=");
        } else if (starts_with(arg, "username=")) {
            opt->username = arg + strlen("username=");
        } else if (starts_with(arg, "password=")) {
            opt->password = arg + strlen("password=");
        } else if (starts_with(arg, "keep_alive=")) {
            if (parse_u32(arg + strlen("keep_alive="), &opt->keep_alive) != 0) {
                return -1;
            }
        } else if (starts_with(arg, "hold_ms=")) {
            if (parse_u32(arg + strlen("hold_ms="), &opt->hold_ms) != 0) {
                return -1;
            }
        } else if (starts_with(arg, "read_timeout_ms=")) {
            if (parse_u32(arg + strlen("read_timeout_ms="), &opt->read_timeout_ms) != 0) {
                return -1;
            }
        } else {
            return -1;
        }
    }

    return 0;
}

int main(int argc, char **argv)
{
    int exit_code = 1;
    int ret;
    unsigned char psk[64];
    size_t psk_len = 0u;
    unsigned char connect_packet[256];
    size_t connect_len = 0u;
    unsigned char connack[4];
    const unsigned char disconnect_packet[2] = { 0xe0u, 0x00u };
    struct options opt;
    mbedtls_net_context server_fd;
    mbedtls_entropy_context entropy;
    mbedtls_ctr_drbg_context ctr_drbg;
    mbedtls_ssl_context ssl;
    mbedtls_ssl_config conf;
    const char *pers = "tls_mqtt_client";

    if (parse_args(argc, argv, &opt) != 0) {
        print_usage(argv[0]);
        return 2;
    }

    if (decode_hex(opt.psk_hex, psk, sizeof(psk), &psk_len) != 0) {
        fprintf(stderr, "Invalid PSK hex\n");
        return 3;
    }

    if (build_connect_packet(&opt, connect_packet, sizeof(connect_packet), &connect_len) != 0) {
        fprintf(stderr, "Failed to build MQTT CONNECT packet\n");
        return 4;
    }

    mbedtls_net_init(&server_fd);
    mbedtls_entropy_init(&entropy);
    mbedtls_ctr_drbg_init(&ctr_drbg);
    mbedtls_ssl_init(&ssl);
    mbedtls_ssl_config_init(&conf);

#if defined(MBEDTLS_USE_PSA_CRYPTO)
    if (psa_crypto_init() != PSA_SUCCESS) {
        fprintf(stderr, "psa_crypto_init failed\n");
        goto cleanup;
    }
#endif

    ret = mbedtls_ctr_drbg_seed(
        &ctr_drbg,
        mbedtls_entropy_func,
        &entropy,
        (const unsigned char *) pers,
        strlen(pers)
    );
    if (ret != 0) {
        print_mbedtls_error("mbedtls_ctr_drbg_seed", ret);
        goto cleanup;
    }

    ret = mbedtls_ssl_config_defaults(
        &conf,
        MBEDTLS_SSL_IS_CLIENT,
        MBEDTLS_SSL_TRANSPORT_STREAM,
        MBEDTLS_SSL_PRESET_DEFAULT
    );
    if (ret != 0) {
        print_mbedtls_error("mbedtls_ssl_config_defaults", ret);
        goto cleanup;
    }

    mbedtls_ssl_conf_authmode(&conf, MBEDTLS_SSL_VERIFY_NONE);
    mbedtls_ssl_conf_rng(&conf, mbedtls_ctr_drbg_random, &ctr_drbg);
    mbedtls_ssl_conf_read_timeout(&conf, opt.read_timeout_ms);
    mbedtls_ssl_conf_tls13_key_exchange_modes(
        &conf,
        MBEDTLS_SSL_TLS1_3_KEY_EXCHANGE_MODE_PSK
    );
    mbedtls_ssl_conf_min_tls_version(&conf, MBEDTLS_SSL_VERSION_TLS1_3);
    mbedtls_ssl_conf_max_tls_version(&conf, MBEDTLS_SSL_VERSION_TLS1_3);

    ret = mbedtls_ssl_conf_psk(
        &conf,
        psk,
        psk_len,
        (const unsigned char *) opt.psk_identity,
        strlen(opt.psk_identity)
    );
    if (ret != 0) {
        print_mbedtls_error("mbedtls_ssl_conf_psk", ret);
        goto cleanup;
    }

    ret = mbedtls_ssl_setup(&ssl, &conf);
    if (ret != 0) {
        print_mbedtls_error("mbedtls_ssl_setup", ret);
        goto cleanup;
    }

    ret = mbedtls_net_connect(&server_fd, opt.server_addr, opt.server_port, MBEDTLS_NET_PROTO_TCP);
    if (ret != 0) {
        print_mbedtls_error("mbedtls_net_connect", ret);
        goto cleanup;
    }

    mbedtls_ssl_set_bio(
        &ssl,
        &server_fd,
        mbedtls_net_send,
        mbedtls_net_recv,
        mbedtls_net_recv_timeout
    );

    do {
        ret = mbedtls_ssl_handshake(&ssl);
    } while (ret == MBEDTLS_ERR_SSL_WANT_READ || ret == MBEDTLS_ERR_SSL_WANT_WRITE);

    if (ret != 0) {
        print_mbedtls_error("mbedtls_ssl_handshake", ret);
        goto cleanup;
    }

    printf("[ Protocol is %s ]\n", mbedtls_ssl_get_version(&ssl));
    printf("[ Ciphersuite is %s ]\n", mbedtls_ssl_get_ciphersuite(&ssl));

    ret = ssl_write_all(&ssl, connect_packet, connect_len);
    if (ret != 0) {
        print_mbedtls_error("mbedtls_ssl_write(CONNECT)", ret);
        goto cleanup;
    }

    ret = ssl_read_exact(&ssl, connack, sizeof(connack));
    if (ret != 0) {
        print_mbedtls_error("mbedtls_ssl_read(CONNACK)", ret);
        goto cleanup;
    }

    if (connack[0] != 0x20u || connack[1] != 0x02u) {
        fprintf(
            stderr,
            "Unexpected CONNACK header: %02X %02X %02X %02X\n",
            connack[0],
            connack[1],
            connack[2],
            connack[3]
        );
        goto cleanup;
    }

    printf(
        "MQTT CONNACK rc=%u session_present=%u client_id=%s\n",
        (unsigned) connack[3],
        (unsigned) (connack[2] & 0x01u),
        opt.client_id
    );

    if (connack[3] != 0u) {
        fprintf(stderr, "MQTT CONNACK rejected with code %u\n", (unsigned) connack[3]);
        goto cleanup;
    }

    printf("MQTT CONNACK accepted for %s\n", opt.client_id);

    if (opt.hold_ms > 0u) {
        sleep_ms(opt.hold_ms);
    }

    ret = ssl_write_all(&ssl, disconnect_packet, sizeof(disconnect_packet));
    if (ret != 0) {
        print_mbedtls_error("mbedtls_ssl_write(DISCONNECT)", ret);
        goto cleanup;
    }

    do {
        ret = mbedtls_ssl_close_notify(&ssl);
    } while (ret == MBEDTLS_ERR_SSL_WANT_READ || ret == MBEDTLS_ERR_SSL_WANT_WRITE);

    exit_code = 0;

cleanup:
    mbedtls_net_free(&server_fd);
    mbedtls_ssl_free(&ssl);
    mbedtls_ssl_config_free(&conf);
    mbedtls_ctr_drbg_free(&ctr_drbg);
    mbedtls_entropy_free(&entropy);
    return exit_code;
}
