/*
 * SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
 * SPDX-License-Identifier: Apache-2.0
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 * http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use prometheus::{Gauge, GaugeVec, IntCounter, Opts, Registry};

use crate::HealthError;
use crate::endpoint::{BmcEndpoint, EndpointMetadata, RackInventory};

const COMPONENT_LABELS: [&str; 11] = [
    "rack_id",
    "session_id",
    "subsystem",
    "component_type",
    "component_uid",
    "bmc_mac",
    "nvl_domain",
    "nmxc_enabled",
    "nmxc_primary",
    "slot_number",
    "tray_index",
];
const RACK_LABELS: [&str; 2] = ["rack_id", "session_id"];
const RACK_DOMAIN_LABELS: [&str; 3] = ["rack_id", "session_id", "nvl_domain"];

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct RackSeries {
    rack_id: String,
    session_id: String,
    created_seconds: i64,
    created_nanos: i32,
}

impl RackSeries {
    fn from_inventory(rack: &RackInventory) -> Option<Self> {
        let created_seconds = rack.created_seconds?;
        let created_nanos = rack.created_nanos?;
        if !(0..1_000_000_000).contains(&created_nanos) {
            tracing::warn!(
                rack_id = %rack.rack_id,
                created_nanos,
                "Skipping rack inventory with an invalid creation timestamp"
            );
            return None;
        }

        let rack_id = rack.rack_id.to_string();
        Some(Self {
            session_id: format!("{rack_id}:{created_seconds}.{created_nanos:09}"),
            rack_id,
            created_seconds,
            created_nanos,
        })
    }

    fn label_values(&self) -> [&str; 2] {
        [&self.rack_id, &self.session_id]
    }

    fn start_time_seconds(&self) -> f64 {
        self.created_seconds as f64 + f64::from(self.created_nanos) / 1_000_000_000.0
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct RackDomainSeries {
    rack_id: String,
    session_id: String,
    nvl_domain: String,
}

impl RackDomainSeries {
    fn label_values(&self) -> [&str; 3] {
        [&self.rack_id, &self.session_id, &self.nvl_domain]
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ComponentSeries {
    rack_id: String,
    session_id: String,
    subsystem: &'static str,
    component_type: &'static str,
    component_uid: String,
    bmc_mac: String,
    nvl_domain: String,
    nmxc_enabled: bool,
    nmxc_primary: bool,
    slot_number: String,
    tray_index: String,
}

impl ComponentSeries {
    fn from_endpoint(endpoint: &BmcEndpoint, rack: &RackSeries) -> Option<Self> {
        let metadata = endpoint.metadata.as_ref()?;
        let (
            subsystem,
            component_uid,
            bmc_mac,
            nvl_domain,
            nmxc_enabled,
            nmxc_primary,
            slot_number,
            tray_index,
        ) = match metadata {
            EndpointMetadata::Machine(machine) => (
                "compute",
                machine.machine_id.as_ref()?.to_string(),
                endpoint.addr.mac.to_string(),
                machine
                    .nvlink_domain_uuid
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_default(),
                false,
                false,
                machine
                    .slot_number
                    .map(|value| value.to_string())
                    .unwrap_or_default(),
                machine
                    .tray_index
                    .map(|value| value.to_string())
                    .unwrap_or_default(),
            ),
            EndpointMetadata::Switch(switch) => (
                "switch",
                switch
                    .id
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| switch.serial.clone()),
                if switch.endpoint_role == crate::endpoint::SwitchEndpointRole::Bmc {
                    endpoint.addr.mac.to_string()
                } else {
                    String::new()
                },
                switch
                    .nvlink_domain_uuid
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_default(),
                switch.nmxc_enabled,
                switch.is_primary,
                switch
                    .slot_number
                    .map(|value| value.to_string())
                    .unwrap_or_default(),
                switch
                    .tray_index
                    .map(|value| value.to_string())
                    .unwrap_or_default(),
            ),
            EndpointMetadata::PowerShelf(power_shelf) => (
                "power",
                power_shelf
                    .id
                    .as_ref()
                    .map(ToString::to_string)
                    .or_else(|| power_shelf.serial.clone())?,
                endpoint.addr.mac.to_string(),
                String::new(),
                false,
                false,
                String::new(),
                String::new(),
            ),
        };

        Some(Self {
            rack_id: rack.rack_id.clone(),
            session_id: rack.session_id.clone(),
            subsystem,
            component_type: metadata.component_type(),
            component_uid,
            bmc_mac,
            nvl_domain,
            nmxc_enabled,
            nmxc_primary,
            slot_number,
            tray_index,
        })
    }

    fn identity(&self) -> (String, String, &'static str, String) {
        (
            self.rack_id.clone(),
            self.session_id.clone(),
            self.component_type,
            self.component_uid.clone(),
        )
    }

    /// Combines metadata from NICo's BMC and host views of the same component.
    ///
    /// Switches can have two endpoints. Only the BMC endpoint contributes the
    /// component-to-BMC identity; the host endpoint can still contribute NMX-C
    /// state. This prevents endpoint discovery order from substituting the NVOS
    /// MAC address for the switch BMC MAC address.
    fn merge(&mut self, other: &Self) {
        debug_assert_eq!(self.identity(), other.identity());

        if self.bmc_mac.is_empty() {
            self.bmc_mac.clone_from(&other.bmc_mac);
        }
        if self.nvl_domain.is_empty() {
            self.nvl_domain.clone_from(&other.nvl_domain);
        }
        if self.slot_number.is_empty() {
            self.slot_number.clone_from(&other.slot_number);
        }
        if self.tray_index.is_empty() {
            self.tray_index.clone_from(&other.tray_index);
        }
        self.nmxc_enabled |= other.nmxc_enabled;
        self.nmxc_primary |= other.nmxc_primary;
    }

    fn label_values(&self) -> [&str; 11] {
        [
            &self.rack_id,
            &self.session_id,
            self.subsystem,
            self.component_type,
            &self.component_uid,
            &self.bmc_mac,
            &self.nvl_domain,
            if self.nmxc_enabled { "true" } else { "false" },
            if self.nmxc_primary { "true" } else { "false" },
            &self.slot_number,
            &self.tray_index,
        ]
    }
}

/// Reconciles the latest successful NICo inventory snapshot into bounded
/// Prometheus info series.
pub(crate) struct InventoryMetrics {
    component_info: GaugeVec,
    rack_nvlink_domain_info: GaugeVec,
    rack_session_start_time_seconds: GaugeVec,
    last_success_time_seconds: Gauge,
    refresh_failures_total: IntCounter,
    current_components: BTreeSet<ComponentSeries>,
    current_rack_domains: BTreeSet<RackDomainSeries>,
    current_racks: BTreeSet<RackSeries>,
}

impl InventoryMetrics {
    pub(crate) fn new(registry: &Registry, metrics_prefix: &str) -> Result<Self, HealthError> {
        let component_info = GaugeVec::new(
            Opts::new(
                format!("{metrics_prefix}_component_inventory_info"),
                "Authoritative NICo component inventory for the current rack-ingestion session",
            ),
            &COMPONENT_LABELS,
        )?;
        registry.register(Box::new(component_info.clone()))?;

        let rack_nvlink_domain_info = GaugeVec::new(
            Opts::new(
                format!("{metrics_prefix}_rack_nvlink_domain_info"),
                "Authoritative NICo rack-to-NVLink-domain assignments for current rack-ingestion sessions",
            ),
            &RACK_DOMAIN_LABELS,
        )?;
        registry.register(Box::new(rack_nvlink_domain_info.clone()))?;

        let rack_session_start_time_seconds = GaugeVec::new(
            Opts::new(
                format!("{metrics_prefix}_rack_session_start_time_seconds"),
                "NICo rack creation time in Unix seconds, labeled by its ingestion session",
            ),
            &RACK_LABELS,
        )?;
        registry.register(Box::new(rack_session_start_time_seconds.clone()))?;

        let last_success_time_seconds = Gauge::new(
            format!("{metrics_prefix}_inventory_last_success_time_seconds"),
            "Unix timestamp of the last successful NICo inventory reconciliation",
        )?;
        registry.register(Box::new(last_success_time_seconds.clone()))?;

        let refresh_failures_total = IntCounter::new(
            format!("{metrics_prefix}_inventory_refresh_failures_total"),
            "Number of failed NICo rack-inventory refreshes",
        )?;
        registry.register(Box::new(refresh_failures_total.clone()))?;

        Ok(Self {
            component_info,
            rack_nvlink_domain_info,
            rack_session_start_time_seconds,
            last_success_time_seconds,
            refresh_failures_total,
            current_components: BTreeSet::new(),
            current_rack_domains: BTreeSet::new(),
            current_racks: BTreeSet::new(),
        })
    }

    pub(crate) fn reconcile(&mut self, racks: &[RackInventory], endpoints: &[Arc<BmcEndpoint>]) {
        self.reconcile_at(racks, endpoints, unix_now_seconds());
    }

    fn reconcile_at(
        &mut self,
        racks: &[RackInventory],
        endpoints: &[Arc<BmcEndpoint>],
        observed_at_seconds: f64,
    ) {
        let desired_racks = racks
            .iter()
            .filter_map(RackSeries::from_inventory)
            .collect::<BTreeSet<_>>();
        let racks_by_id = desired_racks
            .iter()
            .map(|rack| (rack.rack_id.as_str(), rack))
            .collect::<BTreeMap<_, _>>();

        // NICo returns both BMC and host endpoints for a switch. Key by the
        // component identity so those endpoints produce one inventory series.
        let mut desired_by_identity: BTreeMap<_, ComponentSeries> = BTreeMap::new();
        for endpoint in endpoints {
            let Some(rack_id) = endpoint.rack_id.as_ref().map(ToString::to_string) else {
                continue;
            };
            let Some(rack) = racks_by_id.get(rack_id.as_str()) else {
                continue;
            };
            let Some(component) = ComponentSeries::from_endpoint(endpoint, rack) else {
                continue;
            };
            desired_by_identity
                .entry(component.identity())
                .and_modify(|existing| existing.merge(&component))
                .or_insert(component);
        }
        let desired_components = desired_by_identity.into_values().collect::<BTreeSet<_>>();

        // NICo persists a rack-scoped NVLink domain on every active switch in
        // that rack. Collapse matching switch observations into one explicit
        // rack-to-domain relation. Conflicting non-empty domains are a NICo
        // inventory inconsistency, so do not publish an arbitrary assignment.
        let mut domains_by_rack: BTreeMap<_, BTreeSet<_>> = BTreeMap::new();
        for component in &desired_components {
            if component.subsystem == "switch" && !component.nvl_domain.is_empty() {
                domains_by_rack
                    .entry((component.rack_id.clone(), component.session_id.clone()))
                    .or_default()
                    .insert(component.nvl_domain.clone());
            }
        }
        let desired_rack_domains = domains_by_rack
            .into_iter()
            .filter_map(|((rack_id, session_id), domains)| {
                if domains.len() != 1 {
                    tracing::warn!(
                        %rack_id,
                        ?domains,
                        "NICo inventory reports conflicting NVLink domains for one rack"
                    );
                    return None;
                }

                Some(RackDomainSeries {
                    rack_id,
                    session_id,
                    nvl_domain: domains.into_iter().next()?,
                })
            })
            .collect::<BTreeSet<_>>();

        for stale in self.current_components.difference(&desired_components) {
            if let Err(error) = self
                .component_info
                .remove_label_values(&stale.label_values())
            {
                tracing::warn!(?error, "Could not remove stale component inventory metric");
            }
        }
        for component in &desired_components {
            self.component_info
                .with_label_values(&component.label_values())
                .set(1.0);
        }

        for stale in self.current_rack_domains.difference(&desired_rack_domains) {
            if let Err(error) = self
                .rack_nvlink_domain_info
                .remove_label_values(&stale.label_values())
            {
                tracing::warn!(
                    ?error,
                    "Could not remove stale rack-domain inventory metric"
                );
            }
        }
        for rack_domain in &desired_rack_domains {
            self.rack_nvlink_domain_info
                .with_label_values(&rack_domain.label_values())
                .set(1.0);
        }

        for stale in self.current_racks.difference(&desired_racks) {
            if let Err(error) = self
                .rack_session_start_time_seconds
                .remove_label_values(&stale.label_values())
            {
                tracing::warn!(?error, "Could not remove stale rack-session metric");
            }
        }
        for rack in &desired_racks {
            self.rack_session_start_time_seconds
                .with_label_values(&rack.label_values())
                .set(rack.start_time_seconds());
        }

        self.current_components = desired_components;
        self.current_rack_domains = desired_rack_domains;
        self.current_racks = desired_racks;
        self.last_success_time_seconds.set(observed_at_seconds);
    }

    pub(crate) fn record_refresh_failure(&self) {
        self.refresh_failures_total.inc();
    }
}

fn unix_now_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;
    use std::str::FromStr;

    use carbide_uuid::nvlink::NvLinkDomainId;
    use carbide_uuid::rack::RackId;
    use carbide_uuid::switch::{SwitchId, SwitchIdSource, SwitchType};
    use mac_address::MacAddress;
    use prometheus::{Encoder, TextEncoder};

    use super::*;
    use crate::endpoint::test_support::endpoint_with_creds;
    use crate::endpoint::{
        BmcAddr, BmcCredentials, EndpointMetadata, SwitchData, SwitchEndpointRole,
    };

    fn test_switch_id(seed: u8) -> SwitchId {
        SwitchId::new(SwitchIdSource::Tpm, [seed; 32], SwitchType::NvLink)
    }

    fn switch_endpoint(role: SwitchEndpointRole, mac: &str) -> Arc<BmcEndpoint> {
        switch_endpoint_in_rack(role, mac, "D09", 7, "11111111-1111-1111-1111-111111111111")
    }

    fn switch_endpoint_in_rack(
        role: SwitchEndpointRole,
        mac: &str,
        rack_id: &str,
        switch_seed: u8,
        nvl_domain: &str,
    ) -> Arc<BmcEndpoint> {
        Arc::new(endpoint_with_creds(
            BmcAddr {
                ip: IpAddr::from_str("192.0.2.10").unwrap(),
                port: Some(443),
                mac: MacAddress::from_str(mac).unwrap(),
            },
            BmcCredentials::UsernamePassword {
                username: "test".to_string(),
                password: None,
            },
            Some(EndpointMetadata::Switch(SwitchData {
                id: Some(test_switch_id(switch_seed)),
                serial: format!("switch-serial-{switch_seed}"),
                slot_number: Some(9),
                tray_index: Some(3),
                nvlink_domain_uuid: Some(NvLinkDomainId::from_str(nvl_domain).unwrap()),
                endpoint_role: role,
                is_primary: role == SwitchEndpointRole::Host,
                nmxc_enabled: role == SwitchEndpointRole::Host,
                nmxt_enabled: false,
            })),
            Some(RackId::new(rack_id)),
        ))
    }

    fn rack() -> RackInventory {
        rack_with_id("D09", 1_725_000_000)
    }

    fn rack_with_id(rack_id: &str, created_seconds: i64) -> RackInventory {
        RackInventory {
            rack_id: RackId::new(rack_id),
            created_seconds: Some(created_seconds),
            created_nanos: Some(123_000_000),
        }
    }

    fn exposition(registry: &Registry) -> String {
        let mut bytes = Vec::new();
        TextEncoder::new()
            .encode(&registry.gather(), &mut bytes)
            .unwrap();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn reconciles_one_component_for_switch_bmc_and_host_endpoints() {
        let registry = Registry::new();
        let mut metrics = InventoryMetrics::new(&registry, "carbide_hardware_health").unwrap();
        let endpoints = vec![
            switch_endpoint(SwitchEndpointRole::Host, "02:00:00:00:00:02"),
            switch_endpoint(SwitchEndpointRole::Bmc, "02:00:00:00:00:01"),
        ];

        metrics.reconcile_at(&[rack()], &endpoints, 1_800_000_000.0);

        let output = exposition(&registry);
        assert_eq!(
            output
                .lines()
                .filter(|line| line.starts_with("carbide_hardware_health_component_inventory_info{"))
                .count(),
            1
        );
        assert!(output.contains("component_type=\"nvlink_switch\""));
        assert!(output.contains("subsystem=\"switch\""));
        assert!(output.contains("bmc_mac=\"02:00:00:00:00:01\""));
        assert!(!output.contains("bmc_mac=\"02:00:00:00:00:02\""));
        assert!(output.contains("nmxc_enabled=\"true\""));
        assert!(output.contains("nmxc_primary=\"true\""));
        assert!(output.contains(
            "carbide_hardware_health_rack_nvlink_domain_info{nvl_domain=\"11111111-1111-1111-1111-111111111111\",rack_id=\"D09\",session_id=\"D09:1725000000.123000000\"} 1"
        ));
        assert!(output.contains("slot_number=\"9\""));
        assert!(output.contains("tray_index=\"3\""));
        assert!(output.contains("session_id=\"D09:1725000000.123000000\""));
        assert!(output.contains(
            "carbide_hardware_health_rack_session_start_time_seconds{rack_id=\"D09\",session_id=\"D09:1725000000.123000000\"} 1725000000.123"
        ));
        assert!(
            output
                .contains("carbide_hardware_health_inventory_last_success_time_seconds 1800000000")
        );
    }

    #[test]
    fn publishes_one_relation_per_rack_for_a_shared_nvlink_domain() {
        let registry = Registry::new();
        let mut metrics = InventoryMetrics::new(&registry, "carbide_hardware_health").unwrap();
        let domain = "22222222-2222-2222-2222-222222222222";
        let racks = [
            rack_with_id("D09", 1_725_000_000),
            rack_with_id("D10", 1_725_000_100),
        ];
        let endpoints = [
            switch_endpoint_in_rack(
                SwitchEndpointRole::Bmc,
                "02:00:00:00:00:09",
                "D09",
                9,
                domain,
            ),
            switch_endpoint_in_rack(
                SwitchEndpointRole::Bmc,
                "02:00:00:00:00:10",
                "D10",
                10,
                domain,
            ),
        ];

        metrics.reconcile_at(&racks, &endpoints, 1_800_000_000.0);

        let output = exposition(&registry);
        let rack_domains = output
            .lines()
            .filter(|line| line.starts_with("carbide_hardware_health_rack_nvlink_domain_info{"))
            .collect::<Vec<_>>();
        assert_eq!(rack_domains.len(), 2);
        assert!(rack_domains.iter().all(|line| line.contains(domain)));
        assert!(
            rack_domains
                .iter()
                .any(|line| line.contains("rack_id=\"D09\""))
        );
        assert!(
            rack_domains
                .iter()
                .any(|line| line.contains("rack_id=\"D10\""))
        );
    }

    #[test]
    fn replaces_stale_rack_domain_assignment() {
        let registry = Registry::new();
        let mut metrics = InventoryMetrics::new(&registry, "carbide_hardware_health").unwrap();
        let first_domain = "33333333-3333-3333-3333-333333333333";
        let replacement_domain = "44444444-4444-4444-4444-444444444444";

        metrics.reconcile_at(
            &[rack()],
            &[switch_endpoint_in_rack(
                SwitchEndpointRole::Bmc,
                "02:00:00:00:00:01",
                "D09",
                7,
                first_domain,
            )],
            1_800_000_000.0,
        );
        metrics.reconcile_at(
            &[rack()],
            &[switch_endpoint_in_rack(
                SwitchEndpointRole::Bmc,
                "02:00:00:00:00:01",
                "D09",
                7,
                replacement_domain,
            )],
            1_800_000_030.0,
        );

        let output = exposition(&registry);
        assert!(!output.contains(first_domain));
        assert!(output.contains(replacement_domain));
    }

    #[test]
    fn suppresses_conflicting_rack_domain_assignments() {
        let registry = Registry::new();
        let mut metrics = InventoryMetrics::new(&registry, "carbide_hardware_health").unwrap();
        let endpoints = [
            switch_endpoint_in_rack(
                SwitchEndpointRole::Bmc,
                "02:00:00:00:00:01",
                "D09",
                1,
                "55555555-5555-5555-5555-555555555555",
            ),
            switch_endpoint_in_rack(
                SwitchEndpointRole::Bmc,
                "02:00:00:00:00:02",
                "D09",
                2,
                "66666666-6666-6666-6666-666666666666",
            ),
        ];

        metrics.reconcile_at(&[rack()], &endpoints, 1_800_000_000.0);

        let output = exposition(&registry);
        assert!(!output.contains("carbide_hardware_health_rack_nvlink_domain_info{"));
    }

    #[test]
    fn successful_reconciliation_removes_deleted_components() {
        let registry = Registry::new();
        let mut metrics = InventoryMetrics::new(&registry, "carbide_hardware_health").unwrap();
        let endpoint = switch_endpoint(SwitchEndpointRole::Bmc, "02:00:00:00:00:01");

        metrics.reconcile_at(&[rack()], &[endpoint], 1_800_000_000.0);
        metrics.reconcile_at(&[rack()], &[], 1_800_000_030.0);

        let output = exposition(&registry);
        assert!(!output.contains("carbide_hardware_health_component_inventory_info{"));
        assert!(
            output
                .contains("carbide_hardware_health_inventory_last_success_time_seconds 1800000030")
        );
    }

    #[test]
    fn failed_refresh_retains_last_successful_snapshot() {
        let registry = Registry::new();
        let mut metrics = InventoryMetrics::new(&registry, "carbide_hardware_health").unwrap();
        let endpoint = switch_endpoint(SwitchEndpointRole::Bmc, "02:00:00:00:00:01");

        metrics.reconcile_at(&[rack()], &[endpoint], 1_800_000_000.0);
        metrics.record_refresh_failure();

        let output = exposition(&registry);
        assert!(output.contains("carbide_hardware_health_component_inventory_info{"));
        assert!(output.contains("carbide_hardware_health_inventory_refresh_failures_total 1"));
        assert!(
            output
                .contains("carbide_hardware_health_inventory_last_success_time_seconds 1800000000")
        );
    }
}
