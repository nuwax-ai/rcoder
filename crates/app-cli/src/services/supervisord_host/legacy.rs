//! Exact wrapper/configuration provenance for legacy supervisord programs.
use super::*;

pub(super) fn entry_config_from_argv(argv: &[String]) -> Result<PathBuf> {
    let binary = argv.first().context("resident process has no executable")?;
    anyhow::ensure!(
        Path::new(binary)
            .file_name()
            .is_some_and(|name| name == "pingap" || name == "pingap.exe"),
        "resident process executable is not pingap"
    );
    let mut configs = argv
        .windows(2)
        .filter(|pair| pair[0] == "-c")
        .map(|pair| PathBuf::from(&pair[1]));
    let config = configs
        .next()
        .context("resident command has no -c configuration path")?;
    anyhow::ensure!(
        configs.next().is_none() && config.is_absolute(),
        "resident command has ambiguous configuration identity"
    );
    Ok(config)
}

pub(super) fn resident_release_from_fragment(content: &str) -> Result<Option<String>> {
    let mut in_resident = false;
    let mut release = None;
    for line in content.lines().map(str::trim) {
        if line.starts_with('[') {
            in_resident = line == format!("[program:{PINGAP_PROGRAM}]");
        }
        if in_resident && let Some(command) = line.strip_prefix("command=") {
            anyhow::ensure!(
                release.is_none(),
                "multiple resident commands in supervisor fragment"
            );
            let fields: Vec<_> = command.split_whitespace().collect();
            anyhow::ensure!(
                fields.len() == 4
                    && fields[1] == "run-service"
                    && fields[3] == "pingap"
                    && !safe_program_token(fields[2]).is_empty(),
                "resident program has unrecognized wrapper command"
            );
            release = Some(fields[2].to_owned());
        }
    }
    Ok(release)
}

pub(super) fn without_resident_program(content: &str) -> String {
    let mut resident = false;
    let mut output = String::new();
    for line in content.lines() {
        if line.trim().starts_with('[') {
            resident = line.trim() == format!("[program:{PINGAP_PROGRAM}]");
        }
        if !resident {
            output.push_str(line);
            output.push('\n');
        }
    }
    output
}

#[cfg(target_os = "linux")]
pub(super) fn matching_entry_spec(
    conf_path: &Path,
    argv: &[String],
    config: &Path,
) -> Result<Option<ServiceSpecFile>> {
    let resident_conf = conf_path
        .parent()
        .context("supervisor fragment has no parent")?
        .join(RESIDENT_CONF_FILE);
    let mut matched = None;
    for path in [conf_path.to_path_buf(), resident_conf] {
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("read supervisor fragment {}", path.display()));
            }
        };
        let Some(release) = resident_release_from_fragment(&content)? else {
            continue;
        };
        let spec = ServiceSpecFile::load(&release, "pingap")?;
        if spec.argv == argv && entry_config_from_argv(&spec.argv)? == config {
            anyhow::ensure!(
                matched.is_none(),
                "multiple supervisor fragments claim the live resident command"
            );
            matched = Some(spec);
        }
    }
    Ok(matched)
}

pub(super) fn configured_entry_spec(conf_path: &Path) -> Result<Option<ServiceSpecFile>> {
    let resident_conf = conf_path
        .parent()
        .context("supervisor fragment has no parent")?
        .join(RESIDENT_CONF_FILE);
    let mut result = None;
    for path in [conf_path.to_path_buf(), resident_conf] {
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("read supervisor fragment {}", path.display()));
            }
        };
        if let Some(release) = resident_release_from_fragment(&content)? {
            anyhow::ensure!(
                result.is_none(),
                "multiple fragments declare the resident program"
            );
            result = Some(ServiceSpecFile::load(&release, "pingap")?);
        }
    }
    Ok(result)
}
