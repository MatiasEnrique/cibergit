//! Read-only integration probe. Uses explicit existing gh identity and optional local objects.
use anyhow::{Context, Result, ensure};
use cibergit::{
    domain::Account,
    providers::GithubProvider,
    review::{DiffLineKind, local_pr_comparison, parse_file},
};
use std::path::Path;

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    ensure!(
        args.len() == 3 || args.len() == 4,
        "Usage: cargo run --example verify_pr_read -- ACCOUNT OWNER/REPO PR [LOCAL_REPO]"
    );
    let provider = GithubProvider::new(Account {
        host: "github.com".into(),
        login: args[0].clone(),
    });
    let repo = provider.repository(&args[1])?;
    let number: u64 = args[2].parse().context("PR number")?;
    let pr = provider.pull_request(&repo, number)?;
    let remote = provider.comparison(&repo, number, &pr.revision())?;
    println!(
        "Remote: {} #{}; base {}; head {}; files {}; complete {}",
        repo.full_name(),
        number,
        remote.revision.base_sha,
        remote.revision.head_sha,
        remote.files.len(),
        remote.complete
    );
    if let Some(local_path) = args.get(3) {
        let local = local_pr_comparison(Path::new(local_path), &remote.revision)?;
        ensure!(
            local.revision == remote.revision,
            "Selected revisions differ"
        );
        ensure!(
            local.files.len() == remote.files.len(),
            "Changed-file inventories differ"
        );
        for remote_file in &remote.files {
            let local_file = local
                .files
                .iter()
                .find(|f| f.path == remote_file.path)
                .context("Remote file missing locally")?;
            ensure!(
                local_file.status == remote_file.status
                    && local_file.previous_path == remote_file.previous_path,
                "File metadata differs: {}",
                remote_file.path
            );
            if remote_file.patch_complete && local_file.patch_complete {
                let changed = |file| {
                    parse_file(file)
                        .hunks
                        .into_iter()
                        .flat_map(|h| h.lines)
                        .filter(|line| {
                            matches!(line.kind, DiffLineKind::Addition | DiffLineKind::Deletion)
                        })
                        .collect::<Vec<_>>()
                };
                ensure!(
                    changed(remote_file) == changed(local_file),
                    "Changed text or line coordinates differ: {}",
                    remote_file.path
                );
            }
        }
        println!(
            "Local: same immutable revision, file inventory, changed text and line coordinates"
        );
    }
    Ok(())
}
