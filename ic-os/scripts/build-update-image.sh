#!/usr/bin/env bash
#
# Build update image. This is the input to the system updater -- it is
# effectively a gzip'ed tar file that contains the images for the "boot"
# and "root" partitions of the system.

set -eo pipefail

function usage() {
    cat <<EOF
Usage:
  build-update-image -o update.tgz -i ubuntu.dockerimg

  Build update artifact image for IC guest OS. This is a gzip'ed tar file containing
  the boot and root filesystem images for the operating system as well version metadata.

  -i ubuntu.dockerimg: Points to the output of "docker save"
     of the ubuntu docker image. If not given, will implicitly call
     docker build.
  -o update.tgz: Target to write the "update image" to. Use "-" for stdout.
EOF
}

while getopts "i:o:" OPT; do
    case "${OPT}" in
        i)
            IN_FILE="${OPTARG}"
            ;;
        o)
            OUT_FILE="${OPTARG}"
            ;;
        *)
            usage
            exit 1
            ;;
    esac
done

if [ "${OUT_FILE}" == "" ] || [ "${IN_FILE}" == "" ]; then
    usage
    exit 1
fi

TMPDIR=$(mktemp -d)
trap "rm -rf ${TMPDIR}" exit
BASE_DIR=$(dirname "${BASH_SOURCE[0]}")/..

BOOT_IMG="${TMPDIR}"/boot.img
ROOT_IMG="${TMPDIR}"/root.img

"${BASE_DIR}/scripts/build-ubuntu.sh" -i "${IN_FILE}" -r "${ROOT_IMG}" -b "${BOOT_IMG}"
# HACK: allow running without explicitly given version, extract version
# from rootfs. This is NOT good, but workable for the moment.
VERSION=$(debugfs "${ROOT_IMG}" -R "cat /opt/ic/share/version.txt")

echo "${VERSION}" >"${TMPDIR}/VERSION.TXT"
# Sort by name in tar file -- makes ordering deterministic and ensures
# that VERSION.TXT is first entry, making it quick & easy to extract.
# Override owner, group and mtime to make build independent of the user
# building it.
tar czf "${OUT_FILE}" --sort=name --owner=root:0 --group=root:0 --mtime='UTC 2020-01-01' --sparse -C "${TMPDIR}" .

rm -rf "${TMPDIR}"
