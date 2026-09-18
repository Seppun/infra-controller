#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Run this focused regression test from the repository root:
#
#   .github/ci/test-package-rest-helm.sh
#
# It requires helm, yq v4, tar, and sha256sum. It only writes under a temporary
# directory and does not publish an artifact or modify the source chart.

set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
packager="$repo_root/.github/ci/package-rest-helm.sh"
source_chart="$repo_root/helm/rest/nico-rest"
temp_dir=$(mktemp -d)
trap 'rm -rf "$temp_dir"' EXIT

source_digest() {
  find "$source_chart" -type f -print0 \
    | sort -z \
    | xargs -0 sha256sum \
    | sha256sum \
    | cut -d' ' -f1
}

assert_archive() {
  local archive=$1
  local chart_version=$2
  local app_version=$3
  local extract_dir=$4

  mkdir "$extract_dir"
  tar -xzf "$archive" -C "$extract_dir"
  local packaged="$extract_dir/nico-rest"

  [[ $(yq -r '.version' "$packaged/Chart.yaml") == "$chart_version" ]]
  [[ $(yq -r '.appVersion' "$packaged/Chart.yaml") == "$app_version" ]]

  mapfile -t dependencies < <(yq -r '.dependencies[].name' "$packaged/Chart.yaml")
  [[ ${#dependencies[@]} -eq 6 ]]
  local dependency_name
  for dependency_name in "${dependencies[@]}"; do
    [[ $(NAME="$dependency_name" yq -r '.dependencies[] | select(.name == strenv(NAME)) | .version' "$packaged/Chart.yaml") == "$chart_version" ]]
    [[ $(yq -r '.name' "$packaged/charts/$dependency_name/Chart.yaml") == "$dependency_name" ]]
    [[ $(yq -r '.version' "$packaged/charts/$dependency_name/Chart.yaml") == "$chart_version" ]]
    [[ $(yq -r '.appVersion' "$packaged/charts/$dependency_name/Chart.yaml") == "$app_version" ]]
  done
}

initial_digest=$(source_digest)

# These chart/app pairs are the outputs supplied by rest-prepare-build-info.yml.
# The development case proves packaging consumes its existing last-hyphen Helm
# conversion; this script intentionally does not reimplement that conversion.
cases=(
  'exact release tag|2.2.0|v2.2.0'
  'prerelease tag|2.2.0-rc.1|v2.2.0-rc.1'
  'development git describe v2.2.0-3-gabc1234|2.2.0-3.gabc1234|v2.2.0-3-gabc1234'
)

index=0
for case in "${cases[@]}"; do
  IFS='|' read -r name chart_version app_version <<< "$case"
  stage="$temp_dir/stage-$index"
  packages="$temp_dir/packages-$index"
  "$packager" "$source_chart" "$chart_version" "$app_version" "$stage" "$packages"
  mapfile -t archives < <(find "$packages" -maxdepth 1 -name '*.tgz' -type f)
  [[ ${#archives[@]} -eq 1 ]] || { echo "case '$name' did not produce exactly one archive" >&2; exit 1; }
  assert_archive "${archives[0]}" "$chart_version" "$app_version" "$temp_dir/extract-$index"
  [[ $(source_digest) == "$initial_digest" ]] || { echo "case '$name' modified the source chart" >&2; exit 1; }
  printf 'PASS: %s\n' "$name"
  index=$((index + 1))
done

expect_failure() {
  local name=$1
  local expected=$2
  local fixture=$3
  local output

  if output=$("$packager" "$fixture" 2.2.0 v2.2.0 "$temp_dir/$name-stage" "$temp_dir/$name-packages" 2>&1); then
    echo "case '$name' unexpectedly succeeded" >&2
    exit 1
  fi
  [[ "$output" == *"$expected"* ]] || { printf "case '%s' produced the wrong error:\n%s\n" "$name" "$output" >&2; exit 1; }
  printf 'PASS: %s\n' "$name"
}

missing_fixture="$temp_dir/missing"
cp -a "$source_chart" "$missing_fixture"
rm -rf "$missing_fixture/charts/nico-rest-db"
expect_failure missing-dependency "declared dependency is missing its bundled chart: nico-rest-db" "$missing_fixture"

undeclared_fixture="$temp_dir/undeclared"
cp -a "$source_chart" "$undeclared_fixture"
NAME=nico-rest-db yq -i 'del(.dependencies[] | select(.name == strenv(NAME)))' "$undeclared_fixture/Chart.yaml"
expect_failure undeclared-subchart "bundled chart is not declared as a dependency: nico-rest-db" "$undeclared_fixture"

name_fixture="$temp_dir/name-mismatch"
cp -a "$source_chart" "$name_fixture"
yq -i '.name = "wrong-name"' "$name_fixture/charts/nico-rest-db/Chart.yaml"
expect_failure name-mismatch "bundled chart name 'wrong-name' does not match directory 'nico-rest-db'" "$name_fixture"

if output=$("$packager" "$source_chart" v2.2.0 v2.2.0 "$temp_dir/invalid-semver-stage" "$temp_dir/invalid-semver-packages" 2>&1); then
  echo "case 'invalid-semver' unexpectedly succeeded" >&2
  exit 1
fi
[[ "$output" == *"chart version is not strict SemVer: v2.2.0"* ]] || { printf "case 'invalid-semver' produced the wrong error:\n%s\n" "$output" >&2; exit 1; }
printf 'PASS: invalid-semver\n'

[[ $(source_digest) == "$initial_digest" ]] || { echo "packaging tests modified the source chart" >&2; exit 1; }
printf 'PASS: source chart remained unchanged\n'
