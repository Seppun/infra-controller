#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

usage() {
  cat <<'EOF'
Usage: package-rest-helm.sh CHART CHART_VERSION APP_VERSION STAGE_DIR PACKAGE_DIR

Stage the NICo REST umbrella chart, align every bundled chart and dependency to
the supplied versions, package it, and verify the resulting archive. CHART is
never modified. STAGE_DIR must not already exist.
EOF
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

if [[ $# -ne 5 ]]; then
  usage >&2
  exit 2
fi

chart=$1
chart_version=$2
app_version=$3
stage=$4
package_dir=$5

command -v helm >/dev/null 2>&1 || die "helm is required"
command -v yq >/dev/null 2>&1 || die "yq v4 is required"
[[ -f "$chart/Chart.yaml" ]] || die "chart is missing Chart.yaml: $chart"
[[ ! -e "$stage" ]] || die "stage directory already exists: $stage"
[[ -n "$chart_version" ]] || die "chart version must not be empty"
[[ -n "$app_version" ]] || die "app version must not be empty"

semver_number='(0|[1-9][0-9]*)'
semver_identifier='(0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*)'
semver_prerelease="(${semver_identifier})(\\.${semver_identifier})*"
semver_build='[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*'
semver="^${semver_number}\\.${semver_number}\\.${semver_number}(-${semver_prerelease})?(\\+${semver_build})?$"
[[ "$chart_version" =~ $semver ]] || die "chart version is not strict SemVer: $chart_version"

mkdir -p "$(dirname "$stage")" "$package_dir"
cp -a "$chart" "$stage"

root_chart="$stage/Chart.yaml"
charts_dir="$stage/charts"
[[ -d "$charts_dir" ]] || die "umbrella chart is missing its charts directory"

mapfile -t declared_names < <(yq -r '.dependencies[]?.name' "$root_chart")
[[ ${#declared_names[@]} -gt 0 ]] || die "umbrella chart declares no dependencies"

declare -A declared=()
for name in "${declared_names[@]}"; do
  [[ -n "$name" && "$name" != "null" ]] || die "umbrella chart has a dependency without a name"
  [[ -z "${declared[$name]:-}" ]] || die "umbrella chart declares dependency more than once: $name"
  declared[$name]=1
done

declare -A bundled=()
shopt -s nullglob
subchart_files=("$charts_dir"/*/Chart.yaml)
shopt -u nullglob
[[ ${#subchart_files[@]} -gt 0 ]] || die "umbrella chart bundles no subcharts"

for subchart_file in "${subchart_files[@]}"; do
  directory_name=$(basename "$(dirname "$subchart_file")")
  chart_name=$(yq -r '.name' "$subchart_file")
  [[ -n "$chart_name" && "$chart_name" != "null" ]] || die "bundled chart has no name: $subchart_file"
  [[ "$directory_name" == "$chart_name" ]] || die "bundled chart name '$chart_name' does not match directory '$directory_name'"
  [[ -z "${bundled[$chart_name]:-}" ]] || die "bundled chart name is duplicated: $chart_name"
  bundled[$chart_name]=1
done

for name in "${declared_names[@]}"; do
  [[ -n "${bundled[$name]:-}" ]] || die "declared dependency is missing its bundled chart: $name"
done

for name in "${!bundled[@]}"; do
  [[ -n "${declared[$name]:-}" ]] || die "bundled chart is not declared as a dependency: $name"
done

export CHART_VERSION="$chart_version"
export APP_VERSION="$app_version"
yq -i '.version = strenv(CHART_VERSION) | .appVersion = strenv(APP_VERSION) | (.dependencies[].version = strenv(CHART_VERSION))' "$root_chart"

for subchart_file in "${subchart_files[@]}"; do
  yq -i '.version = strenv(CHART_VERSION) | .appVersion = strenv(APP_VERSION)' "$subchart_file"
done

# --dependency-update refreshes the staged Chart.lock after the dependency
# constraints above change. Helm performs its own metadata validation as well.
package_output=$(helm package \
  --destination "$package_dir" \
  --dependency-update \
  --version "$chart_version" \
  --app-version "$app_version" \
  "$stage")
printf '%s\n' "$package_output"

archive=${package_output##*: }
[[ -f "$archive" ]] || die "helm did not produce the reported archive: $archive"

extract_dir=$(mktemp -d)
trap 'rm -rf "$extract_dir"' EXIT
tar -xzf "$archive" -C "$extract_dir"

root_name=$(yq -r '.name' "$root_chart")
packaged_root="$extract_dir/$root_name"
[[ -f "$packaged_root/Chart.yaml" ]] || die "package is missing umbrella Chart.yaml"

[[ $(yq -r '.version' "$packaged_root/Chart.yaml") == "$chart_version" ]] || die "packaged umbrella chart version is incorrect"
[[ $(yq -r '.appVersion' "$packaged_root/Chart.yaml") == "$app_version" ]] || die "packaged umbrella appVersion is incorrect"

for name in "${declared_names[@]}"; do
  dependency_version=$(NAME="$name" yq -r '.dependencies[] | select(.name == strenv(NAME)) | .version' "$packaged_root/Chart.yaml")
  [[ "$dependency_version" == "$chart_version" ]] || die "packaged dependency version is incorrect: $name"

  packaged_subchart="$packaged_root/charts/$name/Chart.yaml"
  [[ -f "$packaged_subchart" ]] || die "package is missing bundled chart: $name"
  [[ $(yq -r '.name' "$packaged_subchart") == "$name" ]] || die "packaged bundled chart name is incorrect: $name"
  [[ $(yq -r '.version' "$packaged_subchart") == "$chart_version" ]] || die "packaged bundled chart version is incorrect: $name"
  [[ $(yq -r '.appVersion' "$packaged_subchart") == "$app_version" ]] || die "packaged bundled chart appVersion is incorrect: $name"
done

printf 'Verified packaged chart: %s\n' "$archive"
