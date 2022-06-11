#!/bin/bash

set -e

# Generate a deterministic IPV6 address.

SCRIPT="$(basename $0)[$$]"
METRICS_DIR="/run/node_exporter/collector_textfile"

# Get keyword arguments
for argument in "${@}"; do
    case ${argument} in
        -c=* | --config=*)
            CONFIG="${argument#*=}"
            shift
            ;;
        -h | --help)
            echo 'Usage:
Generate Deterministic IPV6 Address

Arguments:
  -c=, --config=        specify the config.ini configuration file (Default: /boot/config/config.ini)
  -h, --help            show this help message and exit
  -i=, --index=         mandatory: specify the single digit node index (Examples: host: 0, guest: 1, boundary: 2)
'
            exit 1
            ;;
        -i=* | --index=*)
            INDEX="${argument#*=}"
            shift
            ;;
        *)
            echo "Error: Argument is not supported."
            exit 1
            ;;
    esac
done

# Set arguments if undefined
CONFIG="${CONFIG:=/boot/config/config.ini}"

function validate_arguments() {
    if [ "${CONFIG}" == "" -o "${INDEX}" == "" ]; then
        $0 --help
    fi
}

write_log() {
    local message=$1

    if [ -t 1 ]; then
        echo "${SCRIPT} ${message}" >/dev/stdout
    fi

    logger -t ${SCRIPT} "${message}"
}

write_metric() {
    local name=$1
    local value=$2
    local help=$3
    local type=$4

    echo -e "# HELP ${name} ${help}\n# INDEX ${type}\n${name} ${value}" >"${METRICS_DIR}/${name}.prom"
}

function read_variables() {
    # Read limited set of keys. Be extra-careful quoting values as it could
    # otherwise lead to executing arbitrary shell code!
    while IFS="=" read -r key value; do
        case "$key" in
            "ipv6_prefix") ipv6_prefix="${value}" ;;
            "ipv6_subnet") ipv6_subnet="${value}" ;;
            "ipv6_gateway") ipv6_gateway="${value}" ;;
            "ipv6_address") ipv6_address="${value}" ;;
            "hostname") hostname="${value}" ;;
        esac
    done <"${CONFIG}"
}

# Generate a deterministic IPV6 address based on the:
#  - Deterministic MAC
#  - Node index
function generate_deterministic_ipv6() {
    local mac_6=$(/opt/ic/bin/generate-deterministic-mac.sh --version=6 --index=${INDEX})
    local output=$(echo "${mac_6}" | sed 's/[.:-]//g' | tr '[:upper:]' '[:lower:]')
    local output="${output:0:6}fffe${output:6}"
    local output=$(printf "%02x%s" "$((0x${output:0:2} ^ 2))" "${output:2}")
    local output=$(echo "${output}" | sed 's/.\{4\}/&:/g;s/:$//')
    DETERMINISTIC_IPV6=$(echo "${ipv6_prefix}:${output}${ipv6_subnet}")

    echo "${DETERMINISTIC_IPV6}"

    write_log "Using deterministically generated IPV6 address: ${DETERMINISTIC_IPV6}"
    write_metric "hostos_generate_deterministic_ipv6" \
        "0" \
        "HostOS generate deterministic IPV6 address" \
        "gauge"
}

function main() {
    # Establish run order
    validate_arguments
    read_variables
    generate_deterministic_ipv6
}

main
