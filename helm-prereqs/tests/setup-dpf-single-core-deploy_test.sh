#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SETUP_SH="${SCRIPT_DIR}/../setup.sh"

if [[ "$(grep -Fc 'helm upgrade --install nico ./helm' "${SETUP_SH}")" -ne 1 ]]; then
    echo "setup.sh must define exactly one NICo Core Helm deployment" >&2
    exit 1
fi

if grep -Eq 'DPF_OFF_VALUES|dpf_already_on|rollout restart deployment/nico-api|_dpf_set_bmc_root|NICO_DPF_BMC_ROOT_PASSWORD' "${SETUP_SH}"; then
    echo "setup.sh still contains the obsolete DPF off/on deployment workaround" >&2
    exit 1
fi

if [[ "$(grep -Fc 'kubectl delete job dpf-set-bmc-root' "${SETUP_SH}")" -ne 1 ]] || \
   [[ "$(grep -Fc 'kubectl delete secret dpf-bmc-root-pw dpf-admincli-cert' "${SETUP_SH}")" -ne 1 ]]; then
    echo "setup.sh must remove credentials left by an interrupted legacy DPF bootstrap" >&2
    exit 1
fi

if [[ "$(grep -Fc '_cleanup_legacy_dpf_bootstrap_credentials()' "${SETUP_SH}")" -ne 1 ]] || \
   [[ "$(grep -Fc '        _cleanup_legacy_dpf_bootstrap_credentials >/dev/null 2>&1 || true' "${SETUP_SH}")" -ne 1 ]] || \
   [[ "$(grep -Ec '^_cleanup_legacy_dpf_bootstrap_credentials$' "${SETUP_SH}")" -ne 1 ]]; then
    echo "setup.sh must retry legacy DPF credential cleanup from its EXIT handler" >&2
    exit 1
fi

dpf_prereqs_line="$(grep -nF 'DPF stack installed (Core will start with carbide-api DPF enabled in phase 6)' "${SETUP_SH}" | cut -d: -f1)"
dpf_values_line="$(grep -nF '_CORE_VALUES_ARG="${_DPF_VALUES}"' "${SETUP_SH}" | cut -d: -f1)"
core_deploy_line="$(grep -nF '(cd "${SCRIPT_DIR}/.." && "${NICO_CORE_CMD[@]}")' "${SETUP_SH}" | cut -d: -f1)"

if ! (( dpf_prereqs_line < dpf_values_line && \
        dpf_values_line < core_deploy_line )); then
    echo "DPF prerequisites and enabled values must precede the single Core deploy" >&2
    exit 1
fi

chart_path="${SCRIPT_DIR}/../../helm/charts/nico-api"
default_checksum="$(
    "${HELM:-helm}" template nico-api "${chart_path}" --namespace nico-system |
        awk '/checksum\/config:/ {print $2; exit}'
)"
local_checksum="$(
    "${HELM:-helm}" template nico-api "${chart_path}" --namespace nico-system \
        --set credentials.bmcSiteWideRootSource=local |
        awk '/checksum\/config:/ {print $2; exit}'
)"

if [[ -z "${default_checksum}" || -z "${local_checksum}" || "${default_checksum}" == "${local_checksum}" ]]; then
    echo "nico-api config checksum must change with credential source inputs" >&2
    exit 1
fi

echo "setup DPF single Core deployment test passed"
