//! The `PyPI` half of the policy engine: turning Simple-API records into neutral policy facts.
//!
//! [`velodex_policy::Policy`] is ecosystem-neutral — it decides on a [`FileFacts`] and a normalized
//! project name, never a package format. This module maps `PyPI` `File`s and `ProjectDetail`s into
//! those facts and layers the detail/list filtering on top, exposed as an extension trait so callers
//! keep writing `policy.apply_detail(...)`.

use std::collections::BTreeSet;

use velodex_policy::{FileFacts, PackageType, Policy, PolicyAction, PolicyDenial, retain_versions};

use crate::{DistributionKind, File, ProjectDetail, ProjectList, normalize_name, parse_distribution_filename};

/// Policy operations phrased in `PyPI` terms, implemented on the neutral [`Policy`].
pub trait PypiPolicy {
    /// Check whether one Simple-API file record is allowed.
    ///
    /// # Errors
    /// Returns a denial when the file's parsed facts match a configured policy rule.
    fn check_file(&self, action: PolicyAction, project: &str, file: &File) -> Result<(), PolicyDenial>;

    /// Check whether a direct artifact or metadata download is allowed.
    ///
    /// # Errors
    /// Returns a denial when the filename or known size matches a configured policy rule.
    fn check_download(&self, action: PolicyAction, filename: &str, size: Option<u64>) -> Result<(), PolicyDenial>;

    /// Filter a project detail response through this policy.
    ///
    /// # Errors
    /// Returns a denial when project-wide rules reject the whole response.
    fn apply_detail(
        &self,
        action: PolicyAction,
        project: &str,
        detail: ProjectDetail,
    ) -> Result<ProjectDetail, PolicyDenial>;

    /// Filter a project list to the projects this policy allows.
    fn apply_list(&self, list: ProjectList) -> ProjectList;

    /// Every denial a project detail would raise, for dry-run reporting.
    fn preview_detail(&self, action: PolicyAction, detail: &ProjectDetail) -> Vec<PolicyDenial>;
}

impl PypiPolicy for Policy {
    fn check_file(&self, action: PolicyAction, project: &str, file: &File) -> Result<(), PolicyDenial> {
        self.check_facts(action, &facts_from_file(project, file))
    }

    fn check_download(&self, action: PolicyAction, filename: &str, size: Option<u64>) -> Result<(), PolicyDenial> {
        let artifact = filename.strip_suffix(".metadata").unwrap_or(filename);
        self.check_facts(action, &facts_from_filename(artifact, size))
    }

    fn apply_detail(
        &self,
        action: PolicyAction,
        project: &str,
        mut detail: ProjectDetail,
    ) -> Result<ProjectDetail, PolicyDenial> {
        self.check_project(action, project)?;
        if !self.active() {
            return Ok(detail);
        }
        detail
            .files
            .retain(|file| self.check_file(action, project, file).is_ok());
        if let Some(limit) = self.max_project_size() {
            apply_project_size_limit(action, project, limit, &detail)?;
        }
        retain_versions_with_files(&mut detail);
        Ok(detail)
    }

    fn apply_list(&self, list: ProjectList) -> ProjectList {
        if !self.active() {
            return list;
        }
        ProjectList {
            meta: list.meta,
            projects: list
                .projects
                .into_iter()
                .filter(|entry| {
                    self.check_project(PolicyAction::Serve, &normalize_name(&entry.name))
                        .is_ok()
                })
                .collect(),
        }
    }

    fn preview_detail(&self, action: PolicyAction, detail: &ProjectDetail) -> Vec<PolicyDenial> {
        let mut denials = Vec::new();
        if let Err(denial) = self.check_project(action, &detail.name) {
            denials.push(denial);
            return denials;
        }
        let mut allowed = Vec::new();
        for file in &detail.files {
            match self.check_file(action, &detail.name, file) {
                Ok(()) => allowed.push(file),
                Err(denial) => denials.push(denial),
            }
        }
        if let Some(limit) = self.max_project_size()
            && let Some(denial) = project_size_denial(action, &detail.name, allowed, limit)
        {
            denials.push(denial);
        }
        denials
    }
}

const fn package_type_of(kind: DistributionKind) -> PackageType {
    match kind {
        DistributionKind::Wheel => PackageType::Wheel,
        DistributionKind::SdistTarGz => PackageType::Sdist,
    }
}

fn facts_from_file(project: &str, file: &File) -> FileFacts {
    let parsed = parse_distribution_filename(&file.filename).ok();
    FileFacts {
        project: project.to_owned(),
        filename: Some(file.filename.clone()),
        version: parsed.as_ref().map(|parsed| parsed.version.clone()),
        package_type: parsed.as_ref().map(|parsed| package_type_of(parsed.kind)),
        python_tag: parsed.as_ref().and_then(|parsed| parsed.python_tag.clone()),
        platform_tag: parsed.as_ref().and_then(|parsed| parsed.platform_tag.clone()),
        size: file.size,
    }
}

fn facts_from_filename(filename: &str, size: Option<u64>) -> FileFacts {
    let parsed = parse_distribution_filename(filename).ok();
    FileFacts {
        project: parsed
            .as_ref()
            .map_or_else(|| "<unknown>".to_owned(), |parsed| parsed.normalized_name.clone()),
        filename: Some(filename.to_owned()),
        version: parsed.as_ref().map(|parsed| parsed.version.clone()),
        package_type: parsed.as_ref().map(|parsed| package_type_of(parsed.kind)),
        python_tag: parsed.as_ref().and_then(|parsed| parsed.python_tag.clone()),
        platform_tag: parsed.as_ref().and_then(|parsed| parsed.platform_tag.clone()),
        size,
    }
}

fn apply_project_size_limit(
    action: PolicyAction,
    project: &str,
    limit: u64,
    detail: &ProjectDetail,
) -> Result<(), PolicyDenial> {
    project_size_denial(action, project, detail.files.iter(), limit).map_or(Ok(()), Err)
}

fn project_size_denial<'a>(
    action: PolicyAction,
    project: &str,
    files: impl IntoIterator<Item = &'a File>,
    limit: u64,
) -> Option<PolicyDenial> {
    let mut total = 0_u64;
    for file in files {
        let Some(size) = file.size else {
            return Some(PolicyDenial::new(
                action,
                project,
                Some(&file.filename),
                None,
                "max-project-size",
                "size",
                format!(
                    "project size is unknown because file {:?} has no declared size",
                    file.filename
                ),
            ));
        };
        total = total.saturating_add(size);
    }
    (total > limit).then(|| {
        PolicyDenial::new(
            action,
            project,
            None,
            None,
            "max-project-size",
            "project_size",
            format!("project size {total} exceeds limit {limit}"),
        )
    })
}

fn retain_versions_with_files(detail: &mut ProjectDetail) {
    let versions = detail
        .files
        .iter()
        .filter_map(|file| parse_distribution_filename(&file.filename).ok())
        .map(|parsed| parsed.version.to_string())
        .collect::<BTreeSet<_>>();
    retain_versions(&mut detail.versions, versions);
}
