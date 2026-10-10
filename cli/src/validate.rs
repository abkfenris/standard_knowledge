use std::fs;
use std::path::{Path, PathBuf};
use std::process;

use clap::Args;
use serde::de::DeserializeOwned;
use standard_knowledge::ioos_categories::erddap_categories;
use standard_knowledge::{StandardsLibrary, YamlKnowledge};

#[derive(Args)]
pub struct ValidateArgs {
    /// Knowledge YAML files or directories of them to validate
    #[arg(required = true, value_name = "PATH")]
    paths: Vec<PathBuf>,
}

/// Problems found in a single knowledge file
#[derive(Default)]
struct Report {
    errors: Vec<String>,
    warnings: Vec<String>,
}

pub fn execute(args: &ValidateArgs, library: &StandardsLibrary) {
    let mut files = Vec::new();
    for path in &args.paths {
        if path.is_dir() {
            match knowledge_files_in(path) {
                Ok(mut found) => files.append(&mut found),
                Err(e) => {
                    eprintln!("{}: unable to read directory: {e}", path.display());
                    process::exit(1);
                }
            }
        } else {
            files.push(path.clone());
        }
    }

    let mut failed = 0;
    for file in &files {
        let report = validate_file(file, library);
        for warning in &report.warnings {
            eprintln!("{}: warning: {warning}", file.display());
        }
        for error in &report.errors {
            eprintln!("{}: error: {error}", file.display());
        }
        if !report.errors.is_empty() {
            failed += 1;
        }
    }

    if failed > 0 {
        eprintln!(
            "{failed} of {} knowledge file(s) failed validation",
            files.len()
        );
        process::exit(1);
    }
    println!("{} knowledge file(s) valid", files.len());
}

/// Knowledge YAML files in a directory, skipping the CF standards file
fn knowledge_files_in(path: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(path)? {
        let file = entry?.path();
        let is_yaml = file
            .extension()
            .is_some_and(|ext| ext == "yaml" || ext == "yml");
        let is_cf = file.file_stem().is_some_and(|stem| stem == "_cf_standards");
        if is_yaml && !is_cf {
            files.push(file);
        }
    }
    files.sort();
    Ok(files)
}

fn validate_file(path: &Path, library: &StandardsLibrary) -> Report {
    let mut report = Report::default();

    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(e) => {
            report.errors.push(format!("unable to read file: {e}"));
            return report;
        }
    };

    // Parse loosely first, so syntax errors are reported before structural ones
    // and so we know whether this is a single knowledge or a list of them.
    let value: serde_yaml_ng::Value = match serde_yaml_ng::from_str(&contents) {
        Ok(value) => value,
        Err(e) => {
            report.errors.push(format!("invalid YAML: {e}"));
            return report;
        }
    };

    let filename = path.file_stem().and_then(|stem| stem.to_str());

    match value {
        serde_yaml_ng::Value::Mapping(_) => {
            if let Some(knowledge) = deserialize_strict::<YamlKnowledge>(&contents, &mut report) {
                // Knowledge files name their standard by filename unless `name` is given
                let name = knowledge.name.clone().or(filename.map(String::from));
                check_standard_name_key(&value, name.as_deref(), &mut report);
                check_knowledge(&knowledge, name.as_deref(), library, "", &mut report);
            }
        }
        serde_yaml_ng::Value::Sequence(_) => {
            if let Some(knowledge) =
                deserialize_strict::<Vec<YamlKnowledge>>(&contents, &mut report)
            {
                for (i, know) in knowledge.iter().enumerate() {
                    let prefix = format!("[{i}] ");
                    if know.name.is_none() {
                        report.errors.push(format!(
                            "{prefix}missing `name`, required in a list of knowledge"
                        ));
                    }
                    check_knowledge(know, know.name.as_deref(), library, &prefix, &mut report);
                }
            }
        }
        serde_yaml_ng::Value::Null => report.errors.push("file is empty".to_string()),
        _ => report
            .errors
            .push("expected a mapping of knowledge, or a list of them".to_string()),
    }

    report
}

/// Deserialize, reporting any keys that the knowledge structs don't use.
/// The CLI and library build silently ignore those, so they are usually typos.
fn deserialize_strict<T: DeserializeOwned>(contents: &str, report: &mut Report) -> Option<T> {
    let mut unknown = Vec::new();
    let deserializer = serde_yaml_ng::Deserializer::from_str(contents);
    let result = serde_ignored::deserialize(deserializer, |path| unknown.push(key_path(&path)));

    match result {
        Ok(value) => {
            for key in unknown {
                // `standard_name` is informational and checked separately
                if key != "standard_name" && !key.ends_with("].standard_name") {
                    report.errors.push(format!("unknown key `{key}`"));
                }
            }
            Some(value)
        }
        Err(e) => {
            report.errors.push(format!("invalid knowledge: {e}"));
            None
        }
    }
}

/// Display a key path like `qc.glos.tests`, without serde_ignored's `?` for options
fn key_path(path: &serde_ignored::Path) -> String {
    use serde_ignored::Path;
    match path {
        Path::Root => String::new(),
        Path::Seq { parent, index } => format!("{}[{index}]", key_path(parent)),
        Path::Map { parent, key } => match key_path(parent) {
            parent if parent.is_empty() => key.clone(),
            parent => format!("{parent}.{key}"),
        },
        Path::Some { parent }
        | Path::NewtypeStruct { parent }
        | Path::NewtypeVariant { parent } => key_path(parent),
    }
}

/// The informational `standard_name` key should agree with the standard the knowledge applies to
fn check_standard_name_key(value: &serde_yaml_ng::Value, name: Option<&str>, report: &mut Report) {
    let Some(standard_name) = value.get("standard_name") else {
        return;
    };
    match (standard_name.as_str(), name) {
        (Some(standard_name), Some(name)) if standard_name != name => report.errors.push(format!(
            "`standard_name` is `{standard_name}`, but the knowledge applies to `{name}`"
        )),
        (None, _) => report
            .errors
            .push("`standard_name` should be a string".to_string()),
        _ => {}
    }
}

fn check_knowledge(
    knowledge: &YamlKnowledge,
    name: Option<&str>,
    library: &StandardsLibrary,
    prefix: &str,
    report: &mut Report,
) {
    if let Some(name) = name {
        // Knowledge is only applied to exact CF standard names
        if !library.standards.contains_key(name) {
            report.warnings.push(format!(
                "{prefix}`{name}` is not a CF standard name, so this knowledge will not be applied"
            ));
        }
    }

    for (key, standards) in [
        ("related_standards", &knowledge.related_standards),
        ("sibling_standards", &knowledge.sibling_standards),
    ] {
        for standard in standards.iter().flatten() {
            if library.get(standard).is_err() {
                report.warnings.push(format!(
                    "{prefix}`{standard}` in `{key}` is not a CF standard name or alias"
                ));
            }
        }
    }

    if let Some(category) = &knowledge.ioos_category {
        if !erddap_categories().contains(category.as_str()) {
            report.warnings.push(format!(
                "{prefix}`{category}` is not a known ERDDAP ioos_category"
            ));
        }
    }
}
