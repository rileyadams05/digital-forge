use anyhow::{Context, Error};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::{WordpressStore, WpRendered, decode_entities, map_store_error, strip_html};

const CATALOG_MAX_AGE_SECS: u64 = 24 * 60 * 60;
const CATALOG_PAGE_SIZE: u32 = 100;
const CATALOG_MAX_PAGES: u32 = 200;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct CatalogEntry {
    pub id: u64,
    pub title: String,
    pub page_url: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredCatalog {
    fetched_at: u64,
    entries: Vec<CatalogEntry>,
}

#[derive(Debug, Deserialize)]
struct CatalogPost {
    id: u64,
    link: String,
    title: WpRendered,
}

impl WordpressStore {
    pub fn with_cache(mut self, cache_dir: PathBuf) -> Self {
        self.cache_dir = Some(cache_dir);
        self
    }

    pub fn catalog_is_stale(&self) -> bool {
        match self.load_catalog_file() {
            Some(catalog) => {
                catalog.entries.is_empty()
                    || now_secs().saturating_sub(catalog.fetched_at) > CATALOG_MAX_AGE_SECS
            }
            None => true,
        }
    }

    pub fn ensure_catalog(&self) -> Result<(), Error> {
        self.load_or_refresh_catalog().map(|_| ())
    }

    pub(super) fn load_or_refresh_catalog(&self) -> Result<Vec<CatalogEntry>, Error> {
        if let Some(catalog) = self.load_catalog_file() {
            let fresh = !catalog.entries.is_empty()
                && now_secs().saturating_sub(catalog.fetched_at) <= CATALOG_MAX_AGE_SECS;
            if fresh {
                return Ok(catalog.entries);
            }
            match self.fetch_catalog() {
                Ok(entries) => {
                    self.save_catalog(&entries)?;
                    Ok(entries)
                }
                Err(_) if !catalog.entries.is_empty() => Ok(catalog.entries),
                Err(error) => Err(error),
            }
        } else {
            let entries = self.fetch_catalog()?;
            self.save_catalog(&entries)?;
            Ok(entries)
        }
    }

    fn catalog_path(&self) -> Option<PathBuf> {
        self.cache_dir
            .as_ref()
            .map(|directory| directory.join("store-catalog.json"))
    }

    fn load_catalog_file(&self) -> Option<StoredCatalog> {
        let path = self.catalog_path()?;
        let contents = fs::read_to_string(path).ok()?;
        serde_json::from_str(&contents).ok()
    }

    fn save_catalog(&self, entries: &[CatalogEntry]) -> Result<(), Error> {
        let Some(path) = self.catalog_path() else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).context("could not create the Store cache folder")?;
        }
        let stored = StoredCatalog {
            fetched_at: now_secs(),
            entries: entries.to_vec(),
        };
        let contents =
            serde_json::to_string(&stored).context("could not save the Store title list")?;
        fs::write(path, contents).context("could not save the Store title list")
    }

    fn fetch_catalog(&self) -> Result<Vec<CatalogEntry>, Error> {
        let mut entries = Vec::new();
        let mut page = 1_u32;
        let mut total_pages = 1_u32;
        while page <= total_pages && page <= CATALOG_MAX_PAGES {
            print!("\rLoading Store titles ({page}/{total_pages})...");
            let _ = io::stdout().flush();
            let url = format!(
                "{}/wp-json/wp/v2/posts?per_page={CATALOG_PAGE_SIZE}&page={page}&orderby=id&order=asc&_fields=id,title,link",
                self.base_url
            );
            let response = get_with_retry(&url)?;
            if let Some(value) = response.header("x-wp-totalpages") {
                total_pages = value.parse().unwrap_or(1).max(1);
            }
            let posts: Vec<CatalogPost> = response
                .into_json()
                .context("Store title list returned invalid data")?;
            if posts.is_empty() {
                break;
            }
            entries.extend(posts.into_iter().map(|post| CatalogEntry {
                id: post.id,
                title: strip_html(&decode_entities(&post.title.rendered)),
                page_url: post.link,
            }));
            page += 1;
        }
        println!();
        if entries.is_empty() {
            anyhow::bail!("Search returned no results");
        }
        Ok(entries)
    }
}

fn get_with_retry(url: &str) -> Result<ureq::Response, Error> {
    let mut last_error = None;
    for attempt in 0..4 {
        match WordpressStore::agent().get(url).call() {
            Ok(response) if response.status() < 400 => return Ok(response),
            Ok(_) => anyhow::bail!("Store could not be reached"),
            Err(ureq::Error::Status(429, _)) if attempt < 3 => {
                last_error = Some(anyhow::anyhow!("Store could not be reached"));
                thread::sleep(Duration::from_secs(2 * (attempt as u64 + 1)));
            }
            Err(error) => return Err(map_store_error(error)),
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("Store could not be reached")))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
