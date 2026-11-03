mod background;
mod btm;
mod display;
mod installer;
mod privileged;
mod processes;
mod sync;
mod system;
mod ui;

pub use ui::App;

#[doc(hidden)]
pub fn check_background_store_for_helper(path: &str) -> Result<(), String> {
    btm::check_store_access_as_root(std::path::Path::new(path))
}

#[doc(hidden)]
pub fn remove_background_records_for_helper(
    path: &str,
    uid: u32,
    generated_uid: &str,
    identifiers: &[String],
) -> Result<String, String> {
    let outcome =
        btm::remove_records_as_root(std::path::Path::new(path), uid, generated_uid, identifiers)?;
    Ok(format!(
        "removed={}\nbackup={}",
        outcome.removed_records,
        outcome.backup_path.display()
    ))
}

#[doc(hidden)]
pub fn remove_related_background_records_for_helper(
    uid: u32,
    home: &str,
    identifiers: &[String],
) -> Result<String, String> {
    let outcome =
        btm::remove_related_records_as_root(uid, std::path::Path::new(home), identifiers)?;
    let backups = outcome
        .backups
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(",");
    Ok(format!(
        "removed_records={}\nremoved_services={}\nbackups={backups}",
        outcome.removed_records, outcome.removed_services
    ))
}
