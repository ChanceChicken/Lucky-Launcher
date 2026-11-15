use anyhow::{bail, Context, Result};
use futures::stream::{self, StreamExt};
use reqwest::Client;
use serde_json::Value;
use sha1::{Digest, Sha1};
use std::path::PathBuf;

pub fn client() -> Result<Client> {
    Ok(Client::builder()
        .user_agent(concat!(
            "LuckyLauncher/",
            env!("CARGO_PKG_VERSION"),
            " (personal launcher)"
        ))
        .build()?)
}

pub async fn get_json(client: &Client, url: &str) -> Result<Value> {
    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("request to {url} failed"))?;
    if !resp.status().is_success() {
        bail!("{url} returned {}", resp.status());
    }
    Ok(resp.json().await?)
}

/// Like `get_json`, but a 4xx (not found / bad version) gives `None` instead of an error.
pub async fn try_get_json(client: &Client, url: &str) -> Result<Option<Value>> {
    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("request to {url} failed"))?;
    let status = resp.status();
    if status.as_u16() == 429 {
        bail!("rate limited by {url}; wait a minute and retry");
    }
    if status.is_client_error() {
        return Ok(None);
    }
    if !status.is_success() {
        bail!("{url} returned {status}");
    }
    Ok(Some(resp.json().await?))
}

#[derive(Clone)]
pub struct Job {
    pub url: String,
    pub dest: PathBuf,
    pub sha1: Option<String>,
    pub size: Option<u64>,
}

fn up_to_date(job: &Job) -> bool {
    match std::fs::metadata(&job.dest) {
        Ok(m) => match job.size {
            Some(s) => m.len() == s,
            None => m.len() > 0,
        },
        Err(_) => false,
    }
}

async fn fetch_once(client: &Client, job: &Job) -> Result<()> {
    let resp = client.get(&job.url).send().await?.error_for_status()?;
    let bytes = resp.bytes().await?;
    if let Some(expected) = &job.sha1 {
        let got = hex::encode(Sha1::digest(&bytes));
        if !got.eq_ignore_ascii_case(expected) {
            bail!("hash mismatch for {}", job.url);
        }
    }
    let mut tmp = job.dest.clone().into_os_string();
    tmp.push(".part");
    let tmp = PathBuf::from(tmp);
    tokio::fs::write(&tmp, &bytes).await?;
    tokio::fs::rename(&tmp, &job.dest).await?;
    Ok(())
}

/// Download one file (skipped if it is already there), with retries and hash check.
pub async fn fetch(client: &Client, job: &Job) -> Result<()> {
    if up_to_date(job) {
        return Ok(());
    }
    if let Some(parent) = job.dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut last: Option<anyhow::Error> = None;
    for _ in 0..3 {
        match fetch_once(client, job).await {
            Ok(()) => return Ok(()),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("download failed: {}", job.url)))
}

/// Download many files in parallel; only the missing ones.
pub async fn fetch_all(
    client: &Client,
    mut jobs: Vec<Job>,
    concurrency: usize,
    label: &str,
) -> Result<()> {
    jobs.sort_by(|a, b| a.dest.cmp(&b.dest));
    jobs.dedup_by(|a, b| a.dest == b.dest);
    let todo: Vec<Job> = jobs.into_iter().filter(|j| !up_to_date(j)).collect();
    if todo.is_empty() {
        return Ok(());
    }
    let total = todo.len();
    crate::lprintln!("downloading {total} {label}...");
    let mut results = stream::iter(todo)
        .map(|j| {
            let c = client.clone();
            async move { fetch(&c, &j).await }
        })
        .buffer_unordered(concurrency);
    let mut done = 0usize;
    while let Some(r) = results.next().await {
        r?;
        done += 1;
        if done % 250 == 0 || done == total {
            crate::lprintln!("  {done}/{total}");
        }
    }
    Ok(())
}
