use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail};

use librad::canonical::Cstring;
use librad::identities::payload::{self};
use librad::PeerId;

use rad_common::{git, keys, person, profile, project};
use rad_terminal::args::{Args, Error, Help};
use rad_terminal::components as term;

pub const HELP: Help = Help {
    name: "init",
    description: env!("CARGO_PKG_DESCRIPTION"),
    version: env!("CARGO_PKG_VERSION"),
    usage: r#"
Usage

    rad init [<path>] [<option>...]

Options

    --help    Print help
"#,
};

pub struct Options {
    pub path: Option<PathBuf>,
}

impl Args for Options {
    fn from_args(args: Vec<OsString>) -> anyhow::Result<(Self, Vec<OsString>)> {
        use lexopt::prelude::*;

        let mut parser = lexopt::Parser::from_args(args);
        let mut path: Option<PathBuf> = None;

        if let Some(arg) = parser.next()? {
            match arg {
                Long("help") => {
                    return Err(Error::Help.into());
                }
                Value(val) if path.is_none() => {
                    path = Some(val.into());
                }
                _ => return Err(anyhow::anyhow!(arg.unexpected())),
            }
        }

        Ok((Options { path }, vec![]))
    }
}

pub fn run(options: Options) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let path = options.path.unwrap_or_else(|| cwd.clone());
    let path = path.as_path().canonicalize()?;

    term::headline(&format!(
        "Initializing local 🌱 project in {}",
        if path == cwd {
            term::format::highlight(".")
        } else {
            term::format::highlight(&path.display())
        }
    ));

    execute(path.as_path())
}

pub fn execute(path: &Path) -> anyhow::Result<()> {
    let name = path.file_name().map(|f| f.to_string_lossy().to_string());
    let repo = git::Repository::open(path)?;
    if let Ok(remote) = project::rad_remote(&repo) {
        bail!(
            "repository is already initialized with remote {}",
            remote.url
        );
    }

    let profile = profile::default()?;
    let sock = keys::ssh_auth_sock();
    let (signer, storage) = keys::storage(&profile, sock)?;
    let identity = person::local(&storage)?;

    let head: String = repo
        .head()
        .ok()
        .and_then(|head| head.shorthand().map(|h| h.to_owned()))
        .ok_or_else(|| anyhow!("error: repository head does not point to any commits"))?;

    git::check_version()?;

    let name: String = term::text_input("Name", name)?;
    let description: String = term::text_input("Description", None)?;
    let branch = term::text_input("Default branch", Some(head))?;
    let spinner = term::spinner("Initializing...");

    let payload = payload::Project {
        name: Cstring::from(name),
        description: Some(Cstring::from(description)),
        default_branch: Some(Cstring::from(branch.clone())),
    };

    match project::create(&repo, identity, &storage, signer, &profile, payload) {
        Ok(proj) => {
            let urn = proj.urn();

            spinner.finish();

            // Setup radicle signing key.
            self::setup_signing(storage.peer_id(), &repo)?;

            term::blank();
            term::info!(
                "Your project id is {}. You can show it any time by running:",
                term::format::highlight(&urn.to_string())
            );
            term::indented(&term::format::secondary("rad ."));

            term::blank();
            term::info!("To publish your project to the network, run:");
            term::indented(&term::format::secondary("rad push"));
            term::blank();
        }
        Err(err) => {
            spinner.failed();
            term::blank();

            use rad_common::identities::git::validation;
            use rad_common::identities::git::Error;

            match err.downcast_ref::<Error>() {
                Some(Error::Validation(validation::Error::UrlMismatch { found, .. })) => {
                    bail!(
                        "this repository is already initialized with remote {}",
                        found
                    );
                }
                Some(Error::Validation(validation::Error::MissingDefaultBranch { .. })) => bail!(
                    "the `{}` branch was either not found, or has no commits",
                    branch
                ),
                Some(_) | None => return Err(err),
            }
        }
    }

    Ok(())
}

/// Setup radicle key as commit signing key in repository.
pub fn setup_signing(peer_id: &PeerId, repo: &git::Repository) -> anyhow::Result<()> {
    let repo = repo.workdir().unwrap_or_else(|| repo.path());
    let key = keys::to_ssh_fingerprint(peer_id)?;
    let yes = if !git::is_signing_configured(repo)? {
        term::headline(&format!(
            "Configuring 🌱 signing key {}...",
            term::format::tertiary(key)
        ));
        true
    } else {
        term::confirm(&format!(
            "Configure 🌱 signing key {} in local checkout?",
            term::format::tertiary(key),
        ))
    };

    if yes {
        match git::write_gitsigners(repo, [peer_id]) {
            Ok(file) => {
                git::ignore(repo, file.as_path())?;

                term::success!("Created {} file", term::format::tertiary(file.display()));
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                term::success!(
                    "Found existing {} file",
                    term::format::tertiary(".gitsigners")
                );
                if term::confirm(&format!(
                    "Add signing key to {}?",
                    term::format::tertiary(".gitsigners")
                )) {
                    git::add_gitsigners(repo, [peer_id])?;
                }
            }
            Err(err) => {
                return Err(err.into());
            }
        }
        git::configure_signing(repo, peer_id)?;

        term::success!("Signing key configured");
    }
    Ok(())
}
