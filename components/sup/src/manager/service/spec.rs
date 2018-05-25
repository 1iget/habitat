// Copyright (c) 2016 Chef Software Inc. and/or applicable contributors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::iter::FromIterator;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::result;
use std::str::FromStr;

use glob::glob;
use hcore;
use hcore::package::metadata::BindMapping;
use hcore::package::{PackageIdent, PackageInstall};
use hcore::service::{ApplicationEnvironment, ServiceGroup};
use hcore::util::deserialize_using_from_str;
use protocol;
use serde::{self, Deserialize};
use toml;

use super::composite_spec::CompositeSpec;
use error::{Error, Result, SupError};

static LOGKEY: &'static str = "SS";
const SPEC_FILE_EXT: &'static str = "spec";
const SPEC_FILE_GLOB: &'static str = "*.spec";

pub type BindMap = HashMap<PackageIdent, Vec<BindMapping>>;

pub enum Spec {
    Service(ServiceSpec),
    Composite(CompositeSpec, Vec<ServiceSpec>),
}

impl Spec {
    pub fn ident(&self) -> &PackageIdent {
        match self {
            &Spec::Composite(ref s, _) => s.ident(),
            &Spec::Service(ref s) => s.get_ident(),
        }
    }
}

pub fn deserialize_application_environment<'de, D>(
    d: D,
) -> result::Result<Option<ApplicationEnvironment>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s: Option<String> = Option::deserialize(d)?;
    if let Some(s) = s {
        Ok(Some(FromStr::from_str(&s).map_err(serde::de::Error::custom)?))
    } else {
        Ok(None)
    }
}

pub trait IntoServiceSpec {
    fn into_spec(&self, spec: &mut ServiceSpec);

    /// All specs in a composite currently share a lot of the same
    /// information. Here, we create a "base spec" that we can clone and
    /// further customize for each individual service as needed.
    fn into_composite_spec(
        &self,
        composite_name: String,
        services: Vec<PackageIdent>,
        bind_map: BindMap,
    ) -> Vec<ServiceSpec>;

    fn update_composite(&self, bind_map: &mut BindMap, spec: &mut ServiceSpec);
}

impl IntoServiceSpec for protocol::ctl::SvcLoad {
    fn into_spec(&self, spec: &mut ServiceSpec) {
        spec.set_ident(self.get_ident().clone().into());
        if self.has_group() {
            spec.set_group(self.get_group().to_string());
        }
        if self.has_application_environment() {
            spec.set_application_environment(self.get_application_environment().clone());
        }
        if self.has_bldr_url() {
            spec.set_bldr_url(self.get_bldr_url().to_string());
        }
        if self.has_bldr_channel() {
            spec.set_channel(self.get_bldr_channel().to_string());
        }
        if self.has_topology() {
            spec.set_topology(self.get_topology());
        }
        if self.has_update_strategy() {
            spec.set_update_strategy(self.get_update_strategy());
        }
        if self.has_specified_binds() {
            let (_, standard) = self.get_binds()
                .clone()
                .into_iter()
                .partition(|ref bind| bind.has_service_name());
            spec.set_binds(standard);
        }
        if self.has_binding_mode() {
            spec.set_binding_mode(self.get_binding_mode());
        }
        if self.has_config_from() {
            spec.set_config_from(self.get_config_from().to_string());
        }
        if self.has_svc_encrypted_password() {
            spec.set_svc_encrypted_password(self.get_svc_encrypted_password().to_string());
        }
        spec.clear_composite();
    }

    /// All specs in a composite currently share a lot of the same
    /// information. Here, we create a "base spec" that we can clone and
    /// further customize for each individual service as needed.
    ///
    /// * All services will pull from the same channel in the same
    ///   Builder instance
    /// * All services will be in the same group and app/env. Binds among
    ///   the composite's services are generated based on this
    ///   assumption.
    ///   (We do not set binds here, though, because that requires
    ///   specialized, service-specific handling.)
    /// * For now, all a composite's services will also share the same
    ///   update strategy and topology, though we may want to revisit
    ///   this in the future (particularly for topology).
    fn into_composite_spec(
        &self,
        composite_name: String,
        services: Vec<PackageIdent>,
        mut bind_map: BindMap,
    ) -> Vec<ServiceSpec> {
        // All the service specs will be customized copies of this.
        let mut base_spec = ServiceSpec::default();
        self.into_spec(&mut base_spec);
        base_spec.set_composite(composite_name);
        // TODO (CM): Not dealing with service passwords for now, since
        // that's a Windows-only feature, and we don't currently build
        // Windows composites yet. And we don't have a nice way target
        // them on a per-service basis.
        base_spec.clear_svc_encrypted_password();
        // TODO (CM): Not setting the dev-mode service config_from value
        // because we don't currently have a nice way to target them on a
        // per-service basis.
        base_spec.clear_config_from();

        let composite_binds = if self.has_specified_binds() {
            let binds: Vec<ServiceBind> = self.get_binds()
                .into_iter()
                .map(Clone::clone)
                .map(Into::into)
                .collect();
            let (composite, _) = binds.into_iter().partition(|ref bind| bind.is_composite());
            Some(composite)
        } else {
            None
        };
        let mut specs: Vec<ServiceSpec> = Vec::with_capacity(services.len());
        for service in services {
            // Customize each service's spec as appropriate
            let mut spec = base_spec.clone();
            spec.set_ident(service);
            if let Some(ref binds) = composite_binds {
                set_composite_binds(&mut spec, &mut bind_map, &binds);
            }
            specs.push(spec);
        }
        specs
    }

    fn update_composite(&self, bind_map: &mut BindMap, spec: &mut ServiceSpec) {
        // We only want to update fields that were set by SvcLoad
        if self.has_group() {
            spec.set_group(self.get_group().to_string());
        }
        if self.has_application_environment() {
            spec.set_application_environment(self.get_application_environment().clone());
        }
        if self.has_bldr_url() {
            spec.set_bldr_url(self.get_bldr_url().to_string());
        }
        if self.has_bldr_channel() {
            spec.set_channel(self.get_bldr_channel().to_string());
        }
        if self.has_topology() {
            spec.set_topology(self.get_topology());
        }
        if self.has_update_strategy() {
            spec.set_update_strategy(self.get_update_strategy());
        }
        if self.has_specified_binds() {
            let (composite, standard) = self.get_binds()
                .clone()
                .into_iter()
                .partition(|bind| bind.has_service_name());
            spec.set_binds(standard);
            set_composite_binds(spec, bind_map, &composite);
        }
    }
}

#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct ServiceSpec(protocol::types::ServiceSpec);

impl ServiceSpec {
    pub fn default_for(ident: PackageIdent) -> Self {
        let mut spec = Self::default();
        spec.set_ident(ident.into());
        spec
    }

    pub fn validate(&self, package: &PackageInstall) -> Result<()> {
        self.validate_binds(package)?;
        Ok(())
    }

    /// Validates that all required package binds are present in service binds and all remaining
    /// service binds are optional package binds.
    ///
    /// # Errors
    ///
    /// * If any required required package binds are missing in service binds
    /// * If any given service binds are in neither required nor optional package binds
    fn validate_binds(&self, package: &PackageInstall) -> Result<()> {
        let mut svc_binds: HashSet<String> =
            HashSet::from_iter(self.get_binds().iter().cloned().map(|b| b.get_name()));

        let mut missing_req_binds = Vec::new();
        // Remove each service bind that matches a required package bind. If a required package
        // bind is not found, add the bind to the missing list to return an `Err`.
        for req_bind in package.binds()?.iter().map(|b| &b.service) {
            if svc_binds.contains(req_bind) {
                svc_binds.remove(req_bind);
            } else {
                missing_req_binds.push(req_bind.clone());
            }
        }
        // If we have missing required binds, return an `Err`.
        if !missing_req_binds.is_empty() {
            return Err(sup_error!(Error::MissingRequiredBind(missing_req_binds)));
        }

        // Remove each service bind that matches an optional package bind.
        for opt_bind in package.binds_optional()?.iter().map(|b| &b.service) {
            if svc_binds.contains(opt_bind) {
                svc_binds.remove(opt_bind);
            }
        }
        // If we have remaining service binds then they are neither required nor optional package
        // binds. In this case, return an `Err`.
        if !svc_binds.is_empty() {
            return Err(sup_error!(Error::InvalidBinds(
                svc_binds.into_iter().collect()
            )));
        }

        Ok(())
    }
}

impl Deref for ServiceSpec {
    type Target = protocol::types::ServiceSpec;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for ServiceSpec {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(default)]
pub struct ServiceSpecLegacy {
    #[serde(deserialize_with = "deserialize_using_from_str",
            serialize_with = "serialize_using_to_string")]
    pub ident: PackageIdent,
    pub group: String,
    #[serde(deserialize_with = "deserialize_application_environment",
            skip_serializing_if = "Option::is_none")]
    pub application_environment: Option<ApplicationEnvironment>,
    pub bldr_url: String,
    pub channel: String,
    pub topology: Topology,
    pub update_strategy: UpdateStrategy,
    pub binds: Vec<ServiceBind>,
    #[serde(deserialize_with = "deserialize_using_from_str",
            serialize_with = "serialize_using_to_string")]
    pub binding_mode: BindingMode,
    pub config_from: Option<PathBuf>,
    #[serde(deserialize_with = "deserialize_using_from_str",
            serialize_with = "serialize_using_to_string")]
    pub desired_state: ProcessState,
    pub svc_encrypted_password: Option<String>,
    pub composite: Option<String>,
}

impl ServiceSpecLegacy {
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let file = File::open(&path)
            .map_err(|err| sup_error!(Error::ServiceSpecFileIO(path.as_ref().to_path_buf(), err)))?;
        let mut file = BufReader::new(file);
        let mut buf = String::new();
        file.read_to_string(&mut buf)
            .map_err(|err| sup_error!(Error::ServiceSpecFileIO(path.as_ref().to_path_buf(), err)))?;
        Self::from_str(&buf)
    }

    pub fn file_name(&self) -> String {
        format!("{}.{}", &self.ident.name, SPEC_FILE_EXT)
    }

    pub fn to_latest(self) -> ServiceSpec {
        let mut spec = ServiceSpec::default();
        spec.set_ident(self.ident.into());
        spec.set_group(self.group);
        spec.set_application_environment(self.application_environment);
        spec.set_bldr_url(self.bldr_url);
        spec.set_channel(self.channel);
        spec.set_topology(self.topology);
        spec.set_update_strategy(self.update_strategy);
        spec.set_binds(self.binds.into());
        spec.set_binding_mode(self.binding_mode);
        if let Some(config_from) = self.config_from {
            spec.set_config_from(config_from);
        }
        spec.set_desired_state(self.desired_state);
        if let Some(svc_encrypted_password) = self.svc_encrypted_password {
            spec.set_svc_encrypted_password(svc_encrypted_password);
        }
        if let Some(composite) = self.composite {
            spec.set_composite(composite);
        }
        spec
    }
}

impl FromStr for ServiceSpecLegacy {
    type Err = SupError;

    fn from_str(toml: &str) -> result::Result<Self, Self::Err> {
        let spec: ServiceSpecLegacy =
            toml::from_str(toml).map_err(|e| sup_error!(Error::ServiceSpecParse(e)))?;
        if spec.ident == PackageIdent::default() {
            return Err(sup_error!(Error::MissingRequiredIdent));
        }
        Ok(spec)
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ServiceBind {
    pub name: String,
    pub service_group: ServiceGroup,
    pub service_name: Option<String>,
}

impl ServiceBind {
    pub fn is_composite(&self) -> bool {
        self.service_name.is_some()
    }
}

impl FromStr for ServiceBind {
    type Err = SupError;

    fn from_str(bind_str: &str) -> result::Result<Self, Self::Err> {
        let values: Vec<&str> = bind_str.split(':').collect();
        if !(values.len() == 3 || values.len() == 2) {
            return Err(sup_error!(Error::InvalidBinding(bind_str.to_string())));
        }
        let bind = if values.len() == 3 {
            ServiceBind {
                name: values[1].to_string(),
                service_group: ServiceGroup::from_str(values[2])?,
                service_name: Some(values[0].to_string()),
            }
        } else {
            ServiceBind {
                name: values[0].to_string(),
                service_group: ServiceGroup::from_str(values[1])?,
                service_name: None,
            }
        };
        Ok(bind)
    }
}

impl fmt::Display for ServiceBind {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if let Some(ref service_name) = self.service_name {
            write!(f, "{}:{}:{}", service_name, self.name, self.service_group)
        } else {
            write!(f, "{}:{}", self.name, self.service_group)
        }
    }
}

impl<'de> serde::Deserialize<'de> for ServiceBind {
    fn deserialize<D>(deserializer: D) -> result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserialize_using_from_str(deserializer)
    }
}

impl serde::Serialize for ServiceBind {
    fn serialize<S>(&self, serializer: S) -> result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

pub fn spec_files<T>(watch_path: T) -> Result<Vec<PathBuf>>
where
    T: AsRef<Path>,
{
    Ok(glob(&watch_path
        .as_ref()
        .join(SPEC_FILE_GLOB)
        .display()
        .to_string())?
        .filter_map(|p| p.ok())
        .filter(|p| p.is_file())
        .collect())
}

/// Generate the binds for a composite's service, taking into account
/// both the values laid out in composite definition and any CLI value
/// the user may have specified. This allows the user to override a
/// composite-defined bind, but also (perhaps more usefully) to
/// declare binds for services within the composite that are not
/// themselves *satisfied* by other members of the composite.
///
/// The final list of bind mappings is generated and then set in the
/// `ServiceSpec`. Any binds that may have been present in the spec
/// before are completely ignored.
///
/// # Parameters
///
/// * bind_map: output of package.bind_map()
/// * cli_binds: per-service overrides given on the CLI
fn set_composite_binds(spec: &mut ServiceSpec, bind_map: &mut BindMap, binds: &Vec<ServiceBind>) {
    // We'll be layering bind specifications from the composite
    // with any additional ones from the CLI. We'll store them here,
    // keyed to the bind name
    let mut final_binds: HashMap<String, ServiceBind> = HashMap::new();

    // First, generate the binds from the composite
    if let Some(bind_mappings) = bind_map.remove(spec.get_ident()) {
        // Turn each BindMapping into a ServiceBind

        // NOTE: We are explicitly NOT generating binds that include
        // "organization". This is a feature that never quite found
        // its footing, and will likely be removed / greatly
        // overhauled Real Soon Now (TM) (as of September 2017).
        //
        // As it exists right now, "organization" is a supervisor-wide
        // setting, and thus is only available for `hab sup run`.
        // We don't have a way from `hab svc load` to access the organization setting of an
        // active supervisor, and so we can't generate binds that include organizations.
        for bind_mapping in bind_mappings.iter() {
            let group = ServiceGroup::new(
                spec.get_application_environment(),
                &bind_mapping.satisfying_service.name,
                spec.get_group(),
                None, // <-- organization
            ).expect(
                "Failed to parse bind mapping into service group. Did you validate your input?",
            );
            let bind = ServiceBind {
                name: bind_mapping.bind_name.clone(),
                service_group: group,
                service_name: Some(bind_mapping.bind_name.clone()),
            };
            final_binds.insert(bind.name.clone(), bind);
        }
    }

    // If anything was overridden or added on the CLI, layer that on
    // now as well. These will take precedence over anything in the
    // composite itself.
    //
    // Note that it consumes the values from cli_binds
    for bind in binds
        .iter()
        .filter(|bind| bind.service_name.as_ref().unwrap() == spec.get_ident().get_name())
    {
        final_binds.insert(bind.name.clone(), bind.clone());
    }

    // Now take all the ServiceBinds we've collected.
    spec.set_binds(final_binds.drain().map(|(_, v)| v).collect());
}

#[cfg(test)]
mod test {
    use std::fs::{self, File};
    use std::io::{BufReader, Read, Write};
    use std::path::{Path, PathBuf};
    use std::str::FromStr;

    use hcore::error::Error as HError;
    use hcore::package::PackageIdent;
    use hcore::service::{ApplicationEnvironment, ServiceGroup};
    use tempdir::TempDir;
    use toml;

    use super::*;
    use error::Error::*;

    fn file_from_str<P: AsRef<Path>>(path: P, content: &str) {
        fs::create_dir_all(
            path.as_ref()
                .parent()
                .expect("failed to determine file's parent directory"),
        ).expect("failed to create parent directory recursively");
        let mut file = File::create(path).expect("failed to create file");
        file.write_all(content.as_bytes())
            .expect("failed to write content to file");
    }

    fn string_from_file<P: AsRef<Path>>(path: P) -> String {
        let file = File::open(path).expect("failed to open file");
        let mut file = BufReader::new(file);
        let mut buf = String::new();
        file.read_to_string(&mut buf)
            .expect("cannot read file to string");
        buf
    }

    #[test]
    fn service_spec_from_str() {
        let toml = r#"
            ident = "origin/name/1.2.3/20170223130020"
            group = "jobs"
            application_environment = "theinternet.preprod"
            bldr_url = "http://example.com/depot"
            topology = "leader"
            update_strategy = "rolling"
            binds = ["cache:redis.cache@acmecorp", "db:postgres.app@acmecorp"]
            config_from = "/only/for/development"

            extra_stuff = "should be ignored"
            "#;
        let spec = ServiceSpec::from_str(toml).unwrap();

        assert_eq!(
            spec.ident,
            PackageIdent::from_str("origin/name/1.2.3/20170223130020").unwrap()
        );
        assert_eq!(spec.group, String::from("jobs"));
        assert_eq!(
            spec.application_environment,
            Some(ApplicationEnvironment::from_str("theinternet.preprod").unwrap(),)
        );
        assert_eq!(spec.bldr_url, String::from("http://example.com/depot"));
        assert_eq!(spec.topology, Topology::Leader);
        assert_eq!(spec.update_strategy, UpdateStrategy::Rolling);
        assert_eq!(
            spec.binds,
            vec![
                ServiceBind::from_str("cache:redis.cache@acmecorp").unwrap(),
                ServiceBind::from_str("db:postgres.app@acmecorp").unwrap(),
            ]
        );
        assert_eq!(
            spec.config_from,
            Some(PathBuf::from("/only/for/development"))
        );
    }

    #[test]
    fn service_spec_from_str_missing_ident() {
        let toml = r#""#;

        match ServiceSpec::from_str(toml) {
            Err(e) => match e.err {
                MissingRequiredIdent => assert!(true),
                e => panic!("Unexpected error returned: {:?}", e),
            },
            Ok(_) => panic!("Spec TOML should fail to parse"),
        }
    }

    #[test]
    fn service_spec_from_str_invalid_topology() {
        let toml = r#"
            ident = "origin/name/1.2.3/20170223130020"
            topology = "smartest-possible"
            "#;

        match ServiceSpec::from_str(toml) {
            Err(e) => match e.err {
                ServiceSpecParse(_) => assert!(true),
                e => panic!("Unexpected error returned: {:?}", e),
            },
            Ok(_) => panic!("Spec TOML should fail to parse"),
        }
    }

    #[test]
    fn service_spec_from_str_invalid_binds() {
        let toml = r#"
            ident = "origin/name/1.2.3/20170223130020"
            topology = "leader"
            binds = ["magic:magicness.default", "winning"]
            "#;

        match ServiceSpec::from_str(toml) {
            Err(e) => match e.err {
                ServiceSpecParse(_) => assert!(true),
                e => panic!("Unexpected error returned: {:?}", e),
            },
            Ok(_) => panic!("Spec TOML should fail to parse"),
        }
    }

    #[test]
    fn service_spec_to_toml_string() {
        let spec = ServiceSpec {
            ident: PackageIdent::from_str("origin/name/1.2.3/20170223130020").unwrap(),
            group: String::from("jobs"),
            application_environment: Some(
                ApplicationEnvironment::from_str("theinternet.preprod").unwrap(),
            ),
            bldr_url: String::from("http://example.com/depot"),
            channel: String::from("unstable"),
            topology: Topology::Leader,
            update_strategy: UpdateStrategy::AtOnce,
            binds: vec![
                ServiceBind::from_str("cache:redis.cache@acmecorp").unwrap(),
                ServiceBind::from_str("db:postgres.app@acmecorp").unwrap(),
            ],
            binding_mode: BindingMode::Relaxed,
            config_from: Some(PathBuf::from("/only/for/development")),
            desired_state: ProcessState::Down,
            svc_encrypted_password: None,
            composite: None,
        };
        let toml = spec.to_toml_string().unwrap();

        assert!(toml.contains(r#"ident = "origin/name/1.2.3/20170223130020""#,));
        assert!(toml.contains(r#"group = "jobs""#));
        assert!(toml.contains(r#"application_environment = "theinternet.preprod""#,));
        assert!(toml.contains(r#"bldr_url = "http://example.com/depot""#));
        assert!(toml.contains(r#"channel = "unstable""#));
        assert!(toml.contains(r#"topology = "leader""#));
        assert!(toml.contains(r#"update_strategy = "at-once""#));
        assert!(toml.contains(r#""cache:redis.cache@acmecorp""#));
        assert!(toml.contains(r#""db:postgres.app@acmecorp""#));
        assert!(toml.contains(r#"desired_state = "down""#));
        assert!(toml.contains(r#"config_from = "/only/for/development""#));
        assert!(toml.contains(r#"binding_mode = "relaxed""#));
    }

    #[test]
    fn service_spec_to_toml_string_invalid_ident() {
        // Remember: the default implementation of `PackageIdent` is an invalid identifier, missing
        // origin and name--we're going to exploit this here
        let spec = ServiceSpec::default();

        match spec.to_toml_string() {
            Err(e) => match e.err {
                MissingRequiredIdent => assert!(true),
                wrong => panic!("Unexpected error returned: {:?}", wrong),
            },
            Ok(_) => panic!("Spec TOML should fail to render"),
        }
    }

    #[test]
    fn service_spec_from_file() {
        let tmpdir = TempDir::new("specs").unwrap();
        let path = tmpdir.path().join("name.spec");
        let toml = r#"
            ident = "origin/name/1.2.3/20170223130020"
            group = "jobs"
            application_environment = "theinternet.preprod"
            bldr_url = "http://example.com/depot"
            topology = "leader"
            update_strategy = "rolling"
            binds = ["cache:redis.cache@acmecorp", "db:postgres.app@acmecorp"]
            config_from = "/only/for/development"

            extra_stuff = "should be ignored"
            "#;
        file_from_str(&path, toml);
        let spec = ServiceSpec::from_file(path).unwrap();

        assert_eq!(
            spec.ident,
            PackageIdent::from_str("origin/name/1.2.3/20170223130020").unwrap()
        );
        assert_eq!(spec.group, String::from("jobs"));
        assert_eq!(spec.bldr_url, String::from("http://example.com/depot"));
        assert_eq!(spec.topology, Topology::Leader);
        assert_eq!(
            spec.application_environment,
            Some(ApplicationEnvironment::from_str("theinternet.preprod").unwrap(),)
        );
        assert_eq!(spec.update_strategy, UpdateStrategy::Rolling);
        assert_eq!(
            spec.binds,
            vec![
                ServiceBind::from_str("cache:redis.cache@acmecorp").unwrap(),
                ServiceBind::from_str("db:postgres.app@acmecorp").unwrap(),
            ]
        );
        assert_eq!(&spec.channel, "stable");
        assert_eq!(
            spec.config_from,
            Some(PathBuf::from("/only/for/development"))
        );

        assert_eq!(
            spec.binding_mode,
            BindingMode::Strict,
            "Strict is the default mode, if nothing was previously specified."
        );
    }

    #[test]
    fn service_spec_from_file_missing() {
        let tmpdir = TempDir::new("specs").unwrap();
        let path = tmpdir.path().join("nope.spec");

        match ServiceSpec::from_file(&path) {
            Err(e) => match e.err {
                ServiceSpecFileIO(p, _) => assert_eq!(path, p),
                wrong => panic!("Unexpected error returned: {:?}", wrong),
            },
            Ok(_) => panic!("File should not exist for read"),
        }
    }

    #[test]
    fn service_spec_from_file_empty() {
        let tmpdir = TempDir::new("specs").unwrap();
        let path = tmpdir.path().join("empty.spec");
        file_from_str(&path, "");

        match ServiceSpec::from_file(&path) {
            Err(e) => match e.err {
                MissingRequiredIdent => assert!(true),
                wrong => panic!("Unexpected error returned: {:?}", wrong),
            },
            Ok(_) => panic!("File should not exist for read"),
        }
    }

    #[test]
    fn service_spec_from_file_bad_contents() {
        let tmpdir = TempDir::new("specs").unwrap();
        let path = tmpdir.path().join("bad.spec");
        file_from_str(&path, "You're gonna have a bad time");

        match ServiceSpec::from_file(&path) {
            Err(e) => match e.err {
                ServiceSpecParse(_) => assert!(true),
                wrong => panic!("Unexpected error returned: {:?}", wrong),
            },
            Ok(_) => panic!("File should not exist for read"),
        }
    }

    #[test]
    fn service_spec_to_file() {
        let tmpdir = TempDir::new("specs").unwrap();
        let path = tmpdir.path().join("name.spec");
        let spec = ServiceSpec {
            ident: PackageIdent::from_str("origin/name/1.2.3/20170223130020").unwrap(),
            group: String::from("jobs"),
            application_environment: Some(
                ApplicationEnvironment::from_str("theinternet.preprod").unwrap(),
            ),
            bldr_url: String::from("http://example.com/depot"),
            channel: String::from("unstable"),
            topology: Topology::Leader,
            update_strategy: UpdateStrategy::AtOnce,
            binds: vec![
                ServiceBind::from_str("cache:redis.cache@acmecorp").unwrap(),
                ServiceBind::from_str("db:postgres.app@acmecorp").unwrap(),
            ],
            binding_mode: BindingMode::Relaxed,
            config_from: Some(PathBuf::from("/only/for/development")),
            desired_state: ProcessState::Down,
            svc_encrypted_password: None,
            composite: None,
        };
        spec.to_file(&path).unwrap();
        let toml = string_from_file(path);

        assert!(toml.contains(r#"ident = "origin/name/1.2.3/20170223130020""#,));
        assert!(toml.contains(r#"group = "jobs""#));
        assert!(toml.contains(r#"application_environment = "theinternet.preprod""#,));
        assert!(toml.contains(r#"bldr_url = "http://example.com/depot""#));
        assert!(toml.contains(r#"channel = "unstable""#));
        assert!(toml.contains(r#"topology = "leader""#));
        assert!(toml.contains(r#"update_strategy = "at-once""#));
        assert!(toml.contains(r#""cache:redis.cache@acmecorp""#));
        assert!(toml.contains(r#""db:postgres.app@acmecorp""#));
        assert!(toml.contains(r#"desired_state = "down""#));
        assert!(toml.contains(r#"config_from = "/only/for/development""#));
        assert!(toml.contains(r#"binding_mode = "relaxed""#));
    }

    #[test]
    fn service_spec_to_file_invalid_ident() {
        let tmpdir = TempDir::new("specs").unwrap();
        let path = tmpdir.path().join("name.spec");
        // Remember: the default implementation of `PackageIdent` is an invalid identifier, missing
        // origin and name--we're going to exploit this here
        let spec = ServiceSpec::default();

        match spec.to_file(path) {
            Err(e) => match e.err {
                MissingRequiredIdent => assert!(true),
                wrong => panic!("Unexpected error returned: {:?}", wrong),
            },
            Ok(_) => panic!("Service spec file should not have been written"),
        }
    }

    #[test]
    fn service_spec_file_name() {
        let spec = ServiceSpec::default_for(PackageIdent::from_str("origin/hoopa/1.2.3").unwrap());

        assert_eq!(String::from("hoopa.spec"), spec.file_name());
    }

    #[test]
    fn service_bind_from_str() {
        let bind_str = "name:app.env#service.group@organization";
        let bind = ServiceBind::from_str(bind_str).unwrap();

        assert_eq!(bind.name, String::from("name"));
        assert_eq!(
            bind.service_group,
            ServiceGroup::from_str("app.env#service.group@organization").unwrap()
        );
    }

    #[test]
    fn service_bind_from_str_simple() {
        let bind_str = "name:service.group";
        let bind = ServiceBind::from_str(bind_str).unwrap();

        assert_eq!(bind.name, String::from("name"));
        assert_eq!(
            bind.service_group,
            ServiceGroup::from_str("service.group").unwrap()
        );
    }

    #[test]
    fn service_bind_from_str_missing_colon() {
        let bind_str = "uhoh";

        match ServiceBind::from_str(bind_str) {
            Err(e) => match e.err {
                InvalidBinding(val) => assert_eq!("uhoh", val),
                wrong => panic!("Unexpected error returned: {:?}", wrong),
            },
            Ok(_) => panic!("String should fail to parse"),
        }
    }

    #[test]
    fn service_bind_from_str_too_many_colons() {
        let bind_str = "uhoh:this:is:bad";

        match ServiceBind::from_str(bind_str) {
            Err(e) => match e.err {
                InvalidBinding(val) => assert_eq!("uhoh:this:is:bad", val),
                wrong => panic!("Unexpected error returned: {:?}", wrong),
            },
            Ok(_) => panic!("String should fail to parse"),
        }
    }

    #[test]
    fn service_bind_from_str_invalid_service_group() {
        let bind_str = "uhoh:nosuchservicegroup@nope";

        match ServiceBind::from_str(bind_str) {
            Err(e) => match e.err {
                HabitatCore(HError::InvalidServiceGroup(val)) => {
                    assert_eq!("nosuchservicegroup@nope", val)
                }
                wrong => panic!("Unexpected error returned: {:?}", wrong),
            },
            Ok(_) => panic!("String should fail to parse"),
        }
    }

    #[test]
    fn service_bind_to_string() {
        let bind = ServiceBind {
            name: String::from("name"),
            service_group: ServiceGroup::from_str("service.group").unwrap(),
            service_name: None,
        };

        assert_eq!("name:service.group", bind.to_string());
    }

    #[test]
    fn service_bind_toml_deserialize() {
        #[derive(Deserialize)]
        struct Data {
            key: ServiceBind,
        }
        let toml = r#"
            key = "name:app.env#service.group@organization"
            "#;
        let data: Data = toml::from_str(toml).unwrap();

        assert_eq!(
            data.key,
            ServiceBind::from_str("name:app.env#service.group@organization").unwrap()
        );
    }

    #[test]
    fn service_bind_toml_serialize() {
        #[derive(Serialize)]
        struct Data {
            key: ServiceBind,
        }
        let data = Data {
            key: ServiceBind {
                name: String::from("name"),
                service_group: ServiceGroup::from_str("service.group").unwrap(),
                service_name: None,
            },
        };
        let toml = toml::to_string(&data).unwrap();

        assert!(toml.starts_with(r#"key = "name:service.group""#));
    }
}
