//! The `clouduler registry` commands: add, list, test and remove logins used
//! to pull private images, the cli equivalent of `docker login`.
//!
//! A login is stored as an ordinary cluster secret under a reserved name
//! (see `cl_sched::registries`), so `add` needs this node's cluster key the
//! same way `secret set` does, and `list`/`rm` just talk to a running daemon.
//! Verification reuses exactly what the api's "test" button and pre-save
//! check run, so a login accepted here behaves the same as one saved through
//! the web ui.

use anyhow::{anyhow, bail, Context, Result};
use cl_config::ClusterConfig;
use cl_raft::client::client_for;
use cl_raft::state::Request;
use cl_secret::ClusterKey;
use cl_sched::registries::{normalize_server, secret_name, verify, Registries, StoredLogin};
use tracing::debug;

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .build()
        .context("building the http client")
}

/// Read a password from the terminal without echoing it, or from `--password`
/// / `--password-stdin` when given. Mirrors `docker login`'s flags: a bare
/// prompt for interactive use, `--password-stdin` for scripts and CI, where a
/// prompt would just hang.
fn read_password(password: Option<String>, password_stdin: bool) -> Result<String> {
    if let Some(p) = password {
        return Ok(p);
    }
    if password_stdin {
        let mut buf = String::new();
        std::io::stdin()
            .read_line(&mut buf)
            .context("reading the password from stdin")?;
        let trimmed = buf.trim_end_matches(['\n', '\r']).to_string();
        if trimmed.is_empty() {
            bail!("no password on stdin");
        }
        return Ok(trimmed);
    }
    rpassword::prompt_password("Password: ").context("reading the password from the terminal")
}

/// Check a login against the registry, then seal and store it, replacing any
/// existing login for the same server.
///
/// Verification happens here, on the operator's own machine, before anything
/// is sealed or sent — the same order `secret set` follows for the plaintext
/// itself, and the same check the web ui runs before it will let a save
/// through. A login this rejects never reaches the cluster at all.
pub async fn login(
    cfg: &ClusterConfig,
    server: &str,
    username: &str,
    password: Option<String>,
    password_stdin: bool,
    via: Option<&str>,
) -> Result<()> {
    let server = normalize_server(server);
    if server.is_empty() {
        bail!("the registry host is empty");
    }
    let username = username.trim();
    if username.is_empty() {
        bail!("the username is empty");
    }
    let password = read_password(password, password_stdin)?;
    if password.is_empty() {
        bail!("the password is empty");
    }

    let http = http_client()?;
    verify(&http, &server, username, &password)
        .await
        .map_err(|e| anyhow!(e))?;

    let key = ClusterKey::load(&cfg.secrets.key_file).with_context(|| {
        format!(
            "storing a registry login needs the cluster key at {}. Run this from a node that holds it, or use the web ui",
            cfg.secrets.key_file.display()
        )
    })?;
    let login = StoredLogin {
        server: server.clone(),
        username: username.to_string(),
        password,
    };
    let (name, sealed) = login.seal(&key).with_context(|| format!("sealing the login for {server}"))?;

    let client = client_for(cfg, via)?;
    debug!(server = server.as_str(), "submitting PutSecret for a registry login");
    let response = client
        .submit(Request::PutSecret {
            name,
            sealed: sealed.blob,
            key_fingerprint: key.fingerprint(),
            owner: None,
        })
        .await
        .map_err(|e| anyhow!("{e}"))?;
    if let Some(error) = response.error {
        bail!("cluster rejected the login for {server}: {error}");
    }
    println!("logged in to {server} as {username}");
    Ok(())
}

/// List every registry login the cluster holds. Usernames only: a password
/// never travels back out of `Registries`, on the cli any more than the api.
pub async fn list(cfg: &ClusterConfig, via: Option<&str>) -> Result<()> {
    let key = ClusterKey::load(&cfg.secrets.key_file).with_context(|| {
        format!(
            "listing registry logins needs the cluster key at {}. Run this from a node that holds it, or use the web ui",
            cfg.secrets.key_file.display()
        )
    })?;
    let state = client_for(cfg, via)?
        .state()
        .await
        .map_err(|e| anyhow!("{e}"))?;
    let registries = Registries::load(&state, &key);

    let mut accounts: Vec<(&str, &str)> = registries.accounts().collect();
    if accounts.is_empty() {
        println!("no registry logins stored");
        return Ok(());
    }
    accounts.sort_by(|a, b| a.0.cmp(b.0));
    for (server, username) in accounts {
        println!("{server}\t{username}");
    }
    Ok(())
}

/// Check a stored login against its registry again, without touching what is
/// stored. Catches a password that was rotated on the registry's side, or a
/// login that verified once but no longer does.
pub async fn test(cfg: &ClusterConfig, server: &str, via: Option<&str>) -> Result<()> {
    let server = normalize_server(server);
    let key = ClusterKey::load(&cfg.secrets.key_file).with_context(|| {
        format!(
            "testing a registry login needs the cluster key at {}. Run this from a node that holds it, or use the web ui",
            cfg.secrets.key_file.display()
        )
    })?;
    let state = client_for(cfg, via)?
        .state()
        .await
        .map_err(|e| anyhow!("{e}"))?;
    let registries = Registries::load(&state, &key);
    let creds = registries
        .get(&server)
        .ok_or_else(|| anyhow!("no login stored for {server}"))?;

    let http = http_client()?;
    verify(&http, &server, &creds.username, &creds.password)
        .await
        .map_err(|e| anyhow!(e))?;
    println!("{server} accepted the stored login");
    Ok(())
}

/// Remove a stored registry login. Idempotent the way `secret` removal is
/// not asked to be: a server with nothing stored is not an error, since the
/// end state either way is "no login for this server".
pub async fn remove(cfg: &ClusterConfig, server: &str, via: Option<&str>) -> Result<()> {
    let server = normalize_server(server);
    let client = client_for(cfg, via)?;
    let response = client
        .submit(Request::RemoveSecret {
            name: secret_name(&server),
        })
        .await
        .map_err(|e| anyhow!("{e}"))?;
    if let Some(error) = response.error {
        bail!("cluster rejected removing the login for {server}: {error}");
    }
    println!("removed the login for {server}");
    Ok(())
}
