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

if grep -Eq 'DPF_OFF_VALUES|dpf_already_on|rollout restart deployment/nico-api|_dpf_set_bmc_root' "${SETUP_SH}"; then
    echo "setup.sh still contains the obsolete DPF off/on deployment workaround" >&2
    exit 1
fi

for incompatible_flag in --skip-core --skip-dpf; do
    incompatible_output=""
    if incompatible_output="$(NICO_DPF_BMC_ROOT_PASSWORD=test-only \
            bash "${SETUP_SH}" "${incompatible_flag}" 2>&1)"; then
        echo "setup accepted the BMC bootstrap input with ${incompatible_flag}" >&2
        exit 1
    fi
    if ! grep -Fq "cannot be used with ${incompatible_flag}" <<< "${incompatible_output}"; then
        echo "setup did not explain the BMC bootstrap conflict with ${incompatible_flag}" >&2
        exit 1
    fi
done

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
bmc_secret_line="$(grep -nF '    _prepare_bmc_v0_bootstrap_secret "${_CONFIGURED_CREDENTIAL_FILE_SECRET}"' "${SETUP_SH}" | cut -d: -f1)"
dpf_values_line="$(grep -nF '_CORE_VALUES_ARG="${_DPF_VALUES}"' "${SETUP_SH}" | cut -d: -f1)"
core_deploy_line="$(grep -nF '(cd "${SCRIPT_DIR}/.." && "${NICO_CORE_CMD[@]}")' "${SETUP_SH}" | cut -d: -f1)"

if ! (( dpf_prereqs_line < dpf_values_line && \
        bmc_secret_line < core_deploy_line && dpf_values_line < core_deploy_line )); then
    echo "DPF prerequisites and enabled values must precede the single Core deploy" >&2
    exit 1
fi

credential_secret_parser="$(
    sed -n '/^_nico_api_credential_file_secret_name()/,/^}/p' "${SETUP_SH}"
)"
credential_secret_preparer_untraced="$(
    sed -n '/^_prepare_bmc_v0_bootstrap_secret_untraced()/,/^}/p' "${SETUP_SH}"
)"
credential_secret_preparer="$(
    sed -n '/^_prepare_bmc_v0_bootstrap_secret()/,/^}/p' "${SETUP_SH}"
)"
if [[ -z "${credential_secret_parser}" || \
      -z "${credential_secret_preparer_untraced}" || \
      -z "${credential_secret_preparer}" ]]; then
    echo "could not extract setup-managed BMC credential helpers" >&2
    exit 1
fi
eval "${credential_secret_parser}"
eval "${credential_secret_preparer_untraced}"
eval "${credential_secret_preparer}"

test_tmp_dir="$(mktemp -d)"
trap 'rm -rf "${test_tmp_dir}"' EXIT
test_secret_state="${test_tmp_dir}/secret.json"
test_kubectl_log="${test_tmp_dir}/kubectl.log"
export test_secret_state test_kubectl_log

mkdir -p "${test_tmp_dir}/bin"
cat > "${test_tmp_dir}/bin/kubectl" <<'FAKE_KUBECTL'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "${test_kubectl_log}"

case "$*" in
    'get secret nico-bmc-v0-credentials -n nico-system --ignore-not-found -o json')
        [[ ! -f "${test_secret_state}" ]] || cat "${test_secret_state}"
        ;;
    'create secret generic nico-bmc-v0-credentials -n nico-system --from-file=credentials.yaml=/dev/stdin --dry-run=client -o json')
        encoded="$(base64 | tr -d '\n')"
        jq -n --arg encoded "${encoded}" '{
            apiVersion: "v1",
            kind: "Secret",
            metadata: {name: "nico-bmc-v0-credentials", namespace: "nico-system"},
            data: {"credentials.yaml": $encoded}
        }'
        ;;
    'create -f -')
        tee "${test_secret_state}" >/dev/null
        ;;
    *)
        echo "unexpected kubectl invocation: $*" >&2
        exit 1
        ;;
esac
FAKE_KUBECTL
chmod +x "${test_tmp_dir}/bin/kubectl"

cat > "${test_tmp_dir}/values.yaml" <<'VALUES'
global:
  certificate:
    existingSecret:
      name: unrelated-certificate
'nico-api':
  credentials: {file: {existingSecret: {name: operator-credentials, key: credentials.yaml}}}
VALUES
_CORE_VALUES_INSPECTOR_CHART="${SCRIPT_DIR}/../internal/core-values-inspector"
if [[ "$(_nico_api_credential_file_secret_name "${test_tmp_dir}/values.yaml")" != \
      "operator-credentials" ]]; then
    echo "credential-file Secret parser did not select the nico-api credential path" >&2
    exit 1
fi

cat > "${test_tmp_dir}/null-name-values.yaml" <<'VALUES'
nico-api:
  credentials:
    file:
      existingSecret:
        name: null
VALUES
cat > "${test_tmp_dir}/null-map-values.yaml" <<'VALUES'
nico-api:
  credentials:
    file:
      existingSecret: null
VALUES
for null_values in null-name-values.yaml null-map-values.yaml; do
    if [[ -n "$(_nico_api_credential_file_secret_name \
            "${test_tmp_dir}/${null_values}")" ]]; then
        echo "credential-file Secret parser did not treat ${null_values} as unset" >&2
        exit 1
    fi
done

export PATH="${test_tmp_dir}/bin:${PATH}"
_BMC_V0_BOOTSTRAP_SECRET_NAME="nico-bmc-v0-credentials"
_BMC_V0_BOOTSTRAP_SECRET_KEY="credentials.yaml"
_BMC_V0_BOOTSTRAP_PURPOSE="bmc-site-wide-root-v0"

: > "${test_kubectl_log}"
_BMC_V0_BOOTSTRAP_PASSWORD="initial-password"
_prepare_bmc_v0_bootstrap_secret "nico-bmc-v0-credentials"
if [[ "${_BMC_V0_BOOTSTRAP_ENABLED}" != "true" ]]; then
    echo "new setup-managed BMC Secret was not enabled" >&2
    exit 1
fi
stored_password="$(jq -r '.data["credentials.yaml"]' "${test_secret_state}" | \
    base64 -d | jq -r '.bmc_site_wide_root.password')"
if [[ "${stored_password}" != "initial-password" ]] || \
   [[ "$(jq -r '.metadata.annotations["nico.nvidia.com/credential-purpose"]' \
        "${test_secret_state}")" != "bmc-site-wide-root-v0" ]]; then
    echo "setup-managed BMC Secret has the wrong credential or ownership marker" >&2
    exit 1
fi
if grep -Fq 'initial-password' "${test_kubectl_log}"; then
    echo "BMC password leaked into kubectl arguments" >&2
    exit 1
fi

{
    set -x
    _prepare_bmc_v0_bootstrap_secret "nico-bmc-v0-credentials"
    set +x
} 2>"${test_tmp_dir}/xtrace.log"
if grep -Fq 'initial-password' "${test_tmp_dir}/xtrace.log"; then
    echo "BMC password leaked into setup --debug output" >&2
    exit 1
fi

: > "${test_kubectl_log}"
_prepare_bmc_v0_bootstrap_secret ""
if grep -Fq 'create -f -' "${test_kubectl_log}"; then
    echo "matching BMC bootstrap credential was rewritten" >&2
    exit 1
fi

_BMC_V0_BOOTSTRAP_PASSWORD=""
_prepare_bmc_v0_bootstrap_secret ""
if [[ "${_BMC_V0_BOOTSTRAP_ENABLED}" != "true" ]]; then
    echo "existing setup-managed BMC Secret was not reused without the environment variable" >&2
    exit 1
fi

cp "${test_secret_state}" "${test_tmp_dir}/valid-secret.json"
jq 'del(.data["credentials.yaml"])' "${test_secret_state}" > \
    "${test_tmp_dir}/malformed-secret.json"
mv "${test_tmp_dir}/malformed-secret.json" "${test_secret_state}"
if _prepare_bmc_v0_bootstrap_secret "" 2>"${test_tmp_dir}/malformed.err"; then
    echo "setup reused a malformed setup-managed BMC Secret" >&2
    exit 1
fi
cp "${test_tmp_dir}/valid-secret.json" "${test_secret_state}"

before_mismatch="$(sha256sum "${test_secret_state}")"
_BMC_V0_BOOTSTRAP_PASSWORD="different-password"
if _prepare_bmc_v0_bootstrap_secret "" 2>"${test_tmp_dir}/mismatch.err"; then
    echo "setup accepted a replacement version-0 BMC credential" >&2
    exit 1
fi
if [[ "$(sha256sum "${test_secret_state}")" != "${before_mismatch}" ]]; then
    echo "mismatched version-0 BMC credential changed the existing Secret" >&2
    exit 1
fi

_BMC_V0_BOOTSTRAP_PASSWORD=""
jq 'del(.metadata.annotations)' "${test_secret_state}" > \
    "${test_tmp_dir}/operator-secret.json"
mv "${test_tmp_dir}/operator-secret.json" "${test_secret_state}"
_prepare_bmc_v0_bootstrap_secret "nico-bmc-v0-credentials"
if [[ "${_BMC_V0_BOOTSTRAP_ENABLED}" != "false" ]]; then
    echo "setup adopted an explicitly configured operator-managed BMC Secret" >&2
    exit 1
fi

rm -f "${test_secret_state}"
_BMC_V0_BOOTSTRAP_PASSWORD="initial-password"
if _prepare_bmc_v0_bootstrap_secret "operator-credentials" \
        2>"${test_tmp_dir}/configured.err"; then
    echo "setup replaced an independently configured credential-file Secret" >&2
    exit 1
fi

_BMC_V0_BOOTSTRAP_PASSWORD=""
if _prepare_bmc_v0_bootstrap_secret "nico-bmc-v0-credentials" \
        2>"${test_tmp_dir}/missing.err"; then
    echo "setup accepted a missing configured credential-file Secret" >&2
    exit 1
fi

_BMC_V0_BOOTSTRAP_PASSWORD=$'line-ending\n'
_prepare_bmc_v0_bootstrap_secret "nico-bmc-v0-credentials"
_prepare_bmc_v0_bootstrap_secret "nico-bmc-v0-credentials"

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
