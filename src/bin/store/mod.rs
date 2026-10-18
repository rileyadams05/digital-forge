pub mod browser;
mod catalog;
pub mod download;
pub mod mega_download;

use anyhow::{Context, Error, anyhow};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::path::PathBuf;
use std::time::Duration;

pub const DEFAULT_STORE_BASE: &str = "https://dlxbgame.com";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveFormat {
    Iso,
    Zip,
    Rar,
    SevenZip,
    MultipartRar,
    MultipartZip,
    MultipartSevenZip,
    Jtag,
    Unknown,
}

impl ArchiveFormat {
    pub fn label(self) -> &'static str {
        match self {
            Self::Iso => "ISO",
            Self::Zip => "ZIP",
            Self::Rar => "RAR",
            Self::SevenZip => "7Z",
            Self::MultipartRar => "Multipart RAR",
            Self::MultipartZip => "Multipart ZIP",
            Self::MultipartSevenZip => "Multipart 7Z",
            Self::Jtag => "Extracted game",
            Self::Unknown => "Unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DownloadAccess {
    Direct,
    BrowserAuthentication,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadOption {
    pub label: String,
    pub url: String,
    pub urls: Vec<String>,
    pub password: Option<String>,
}

/// The one explicitly excluded host; other choices are not availability-filtered.
pub fn is_onefile_option(option: &DownloadOption) -> bool {
    let label = option.label.to_ascii_lowercase().replace(' ', "");
    matches!(label.as_str(), "1file" | "1fichier")
        || download::option_urls(option).iter().any(|url| {
            reqwest::Url::parse(url).ok().is_some_and(|url| {
                matches!(url.host_str(), Some("1fichier.com" | "www.1fichier.com"))
            })
        })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreListing {
    pub id: String,
    pub title: String,
    pub format: ArchiveFormat,
    pub uploader: String,
    pub size: Option<String>,
    pub region: Option<String>,
    pub parts: Option<u32>,
    pub page_url: String,
    pub password_required: bool,
    pub access: DownloadAccess,
    pub direct_urls: Vec<String>,
    pub download_options: Vec<DownloadOption>,
}

pub trait StoreAdapter {
    fn search(&self, query: &str) -> Result<Vec<StoreListing>, Error>;
    fn inspect(&self, listing: &StoreListing) -> Result<StoreListing, Error>;
}

pub struct WordpressStore {
    pub base_url: String,
    cache_dir: Option<PathBuf>,
}

impl WordpressStore {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            cache_dir: None,
        }
    }

    fn agent() -> ureq::Agent {
        ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(20))
            .timeout_read(Duration::from_secs(45))
            .user_agent("ISO2GOD/2.0 (authorized personal backup helper)")
            .build()
    }

    fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T, Error> {
        let response = Self::agent().get(url).call().map_err(map_store_error)?;
        if response.status() >= 400 {
            anyhow::bail!("Store could not be reached");
        }
        response
            .into_json()
            .context("Store search returned invalid data")
    }

    fn search_posts(&self, query: &str) -> Result<Vec<WpPost>, Error> {
        let mut posts = Vec::new();
        let mut page = 1_u32;
        let mut total_pages = 1_u32;
        while page <= total_pages && page <= 5 {
            let url = format!(
                "{}/wp-json/wp/v2/posts?search={}&per_page=100&page={page}&orderby=relevance&_fields=id,link,title,content,author,class_list,yoast_head_json",
                self.base_url,
                percent_encode(query)
            );
            let response = Self::agent().get(&url).call().map_err(map_store_error)?;
            if response.status() >= 400 {
                anyhow::bail!("Store could not be reached");
            }
            if let Some(value) = response.header("x-wp-totalpages") {
                total_pages = value.parse().unwrap_or(1).max(1);
            }
            let page_posts: Vec<WpPost> = response
                .into_json()
                .context("Store search returned invalid data")?;
            if page_posts.is_empty() {
                break;
            }
            posts.extend(page_posts);
            page += 1;
        }
        Ok(posts)
    }
}

impl StoreAdapter for WordpressStore {
    fn search(&self, query: &str) -> Result<Vec<StoreListing>, Error> {
        let query = query.trim();
        if query.is_empty() {
            anyhow::bail!("Search returned no results");
        }
        let mut listings = match self.load_or_refresh_catalog() {
            Ok(entries) => entries
                .into_iter()
                .filter(|entry| title_matches_query(&entry.title, query))
                .map(listing_from_catalog)
                .collect::<Vec<_>>(),
            Err(_) => self
                .search_posts(query)?
                .into_iter()
                .map(listing_from_post)
                .filter(|listing| title_matches_query(&listing.title, query))
                .collect::<Vec<_>>(),
        };
        prefer_prepared_listings(&mut listings);
        listings.sort_by(|left, right| compare_search_relevance(&left.title, &right.title, query));
        if listings.is_empty() {
            anyhow::bail!("Search returned no results");
        }
        Ok(listings)
    }

    fn inspect(&self, listing: &StoreListing) -> Result<StoreListing, Error> {
        let url = format!(
            "{}/wp-json/wp/v2/posts/{}?_fields=id,link,title,content,author,class_list,yoast_head_json",
            self.base_url,
            percent_encode(&listing.id)
        );
        let post: WpPost = self.get_json(&url)?;
        Ok(listing_from_post(post))
    }
}

// Archive containers (including multipart RAR) do not identify the game format.
// Only suppress an ISO when the catalogue explicitly identifies a prepared copy
// of the same title/edition; unknown ZIP/RAR listings are not sufficient evidence.
fn prefer_prepared_listings(listings: &mut Vec<StoreListing>) {
    let prepared: std::collections::HashSet<String> = listings
        .iter()
        .filter(|listing| {
            let words = normalized_words(&listing.title);
            listing.format == ArchiveFormat::Jtag
                || words
                    .iter()
                    .any(|word| matches!(word.as_str(), "jtag" | "rgh"))
        })
        .map(|listing| game_title_key(&listing.title))
        .filter(|key| !key.is_empty())
        .collect();
    listings.retain(|listing| {
        listing.format != ArchiveFormat::Iso || !prepared.contains(&game_title_key(&listing.title))
    });
}

fn game_title_key(title: &str) -> String {
    let mut name = String::new();
    let mut remaining = title;
    while let Some(start) = remaining.find('[') {
        name.push_str(&remaining[..start]);
        let Some(end) = remaining[start..].find(']') else {
            name.push_str(&remaining[start..]);
            remaining = "";
            break;
        };
        let end = start + end;
        let tag = normalized_words(&remaining[start + 1..end]).join("");
        if !matches!(
            tag.as_str(),
            "iso" | "jtag" | "rgh" | "jtagrgh" | "regionfree" | "pal" | "ntsc" | "ntscu" | "ntscj"
        ) {
            name.push_str(&remaining[start..=end]);
        }
        name.push(' ');
        remaining = &remaining[end + 1..];
    }
    name.push_str(remaining);
    let mut words = normalized_words(&name);
    while words
        .last()
        .is_some_and(|word| matches!(word.as_str(), "iso" | "jtag" | "rgh"))
    {
        words.pop();
    }
    words.join(" ")
}

fn map_store_error(error: ureq::Error) -> Error {
    match error {
        ureq::Error::Status(403, _) | ureq::Error::Status(401, _) | ureq::Error::Status(429, _) => {
            anyhow!("Store could not be reached")
        }
        ureq::Error::Status(status, _) if status >= 500 => anyhow!("Store could not be reached"),
        ureq::Error::Status(404, _) => anyhow!("Search returned no results"),
        other => anyhow!("Store could not be reached: {other}"),
    }
}

#[derive(Debug, Deserialize)]
struct WpPost {
    id: u64,
    link: String,
    title: WpRendered,
    #[serde(default)]
    content: Option<WpRendered>,
    #[serde(default)]
    author: Option<u64>,
    #[serde(default)]
    class_list: Option<Vec<String>>,
    #[serde(default)]
    yoast_head_json: Option<WpYoast>,
}

#[derive(Debug, Deserialize)]
struct WpRendered {
    rendered: String,
}

#[derive(Debug, Deserialize)]
struct WpYoast {
    #[serde(default)]
    author: Option<String>,
}

const SERIES_ALIASES: &[(&str, &str)] = &[
    ("gta", "Grand Theft Auto"),
    ("cod", "Call of Duty"),
    ("rdr", "Red Dead Redemption"),
    ("nfs", "Need for Speed"),
    ("gow", "Gears of War"),
    ("ac", "Assassin's Creed"),
    ("bf", "Battlefield"),
    ("mk", "Mortal Kombat"),
    ("re", "Resident Evil"),
    ("bo", "Black Ops"),
    ("mw", "Modern Warfare"),
];

fn search_variants(query: &str) -> Vec<String> {
    let query = query.trim();
    let mut variants = vec![query.to_owned()];
    let words = normalized_words(query);
    for (short, full) in SERIES_ALIASES {
        let short_words = normalized_words(short);
        if short_words.is_empty() {
            continue;
        }
        let Some(index) = words
            .windows(short_words.len())
            .position(|window| window == short_words.as_slice())
        else {
            continue;
        };
        if !variants
            .iter()
            .any(|variant| variant.eq_ignore_ascii_case(full))
        {
            variants.push((*full).to_owned());
        }
        let mut expanded = words.clone();
        expanded.splice(index..index + short_words.len(), normalized_words(full));
        let expanded = expanded.join(" ");
        if !variants
            .iter()
            .any(|variant| variant.eq_ignore_ascii_case(&expanded))
        {
            variants.push(expanded);
        }
    }
    variants
}

fn title_matches_query(title: &str, query: &str) -> bool {
    let title_words = normalized_words(title);
    let title_lower = title.to_ascii_lowercase();
    search_variants(query).iter().any(|variant| {
        let variant_lower = variant.to_ascii_lowercase();
        if !variant_lower.is_empty() && title_lower.contains(&variant_lower) {
            return true;
        }
        let words = normalized_words(variant);
        !words.is_empty()
            && words
                .iter()
                .all(|word| title_has_search_word(&title_words, word))
    })
}

fn title_has_search_word(title_words: &[String], word: &str) -> bool {
    title_words
        .iter()
        .any(|title_word| title_word == word || (word.len() >= 2 && title_word.starts_with(word)))
}

fn compare_search_relevance(left: &str, right: &str, query: &str) -> Ordering {
    relevance_score(right, query)
        .cmp(&relevance_score(left, query))
        .then_with(|| left.to_ascii_lowercase().cmp(&right.to_ascii_lowercase()))
}

fn relevance_score(title: &str, query: &str) -> u8 {
    let title_lower = title.to_ascii_lowercase();
    let query_lower = query.trim().to_ascii_lowercase();
    if !query_lower.is_empty() && title_lower.starts_with(&query_lower) {
        return 3;
    }
    if !query_lower.is_empty() && title_lower.contains(&query_lower) {
        return 2;
    }
    u8::from(search_variants(query).iter().any(|variant| {
        let variant_lower = variant.to_ascii_lowercase();
        !variant_lower.is_empty() && title_lower.contains(&variant_lower)
    }))
}

fn normalized_words(text: &str) -> Vec<String> {
    text.to_ascii_lowercase()
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_owned)
        .collect()
}

fn listing_from_catalog(entry: catalog::CatalogEntry) -> StoreListing {
    let format = infer_format(&entry.title, None, &[], "");
    let region = infer_region(&entry.title, None);
    let parts = infer_parts("", &entry.title);
    StoreListing {
        id: entry.id.to_string(),
        title: entry.title,
        format,
        uploader: String::new(),
        size: None,
        region,
        parts,
        page_url: entry.page_url,
        password_required: false,
        access: DownloadAccess::BrowserAuthentication,
        direct_urls: Vec::new(),
        download_options: Vec::new(),
    }
}

fn listing_from_post(post: WpPost) -> StoreListing {
    let title = strip_html(&decode_entities(&post.title.rendered));
    let content = post
        .content
        .as_ref()
        .map(|content| decode_entities(&content.rendered))
        .unwrap_or_default();
    let links = extract_hrefs(&content);
    let (access, direct_urls) = classify_links(&links);
    let download_options = parse_download_options(&content);
    let password_required = content_mentions_password(&content)
        || download_options
            .iter()
            .any(|option| option.password.is_some());
    let format = infer_format(&title, post.class_list.as_deref(), &links, &content);
    let region = infer_region(&title, post.class_list.as_deref());
    let uploader = infer_uploader(post.yoast_head_json.as_ref(), post.author, &content);
    let size = infer_size(&content);
    let parts = infer_parts(&content, &title);
    StoreListing {
        id: post.id.to_string(),
        title,
        format,
        uploader,
        size,
        region,
        parts,
        page_url: post.link,
        password_required,
        access,
        direct_urls,
        download_options,
    }
}

pub fn infer_format(
    title: &str,
    class_list: Option<&[String]>,
    links: &[String],
    content: &str,
) -> ArchiveFormat {
    let haystack = format!(
        "{} {}",
        title.to_ascii_lowercase(),
        class_list
            .unwrap_or(&[])
            .iter()
            .map(|value| value.to_ascii_lowercase())
            .collect::<Vec<_>>()
            .join(" ")
    );
    let part_count = infer_parts(content, title).unwrap_or(0);
    let link_blob = links.join(" ").to_ascii_lowercase();
    if haystack.contains("[iso]") || haystack.contains("category-iso") {
        return ArchiveFormat::Iso;
    }
    if haystack.contains("jtag") || haystack.contains("rgh") {
        if part_count > 1 || looks_multipart(&link_blob, content) {
            return ArchiveFormat::MultipartRar;
        }
        return ArchiveFormat::Jtag;
    }
    if looks_multipart(&link_blob, content) || part_count > 1 {
        if link_blob.contains(".7z") || content.to_ascii_lowercase().contains(".7z") {
            return ArchiveFormat::MultipartSevenZip;
        }
        if link_blob.contains(".zip") {
            return ArchiveFormat::MultipartZip;
        }
        return ArchiveFormat::MultipartRar;
    }
    if haystack.contains("[iso]") || haystack.contains(" iso") || haystack.contains("category-iso")
    {
        return ArchiveFormat::Iso;
    }
    if link_blob.contains(".7z") {
        return ArchiveFormat::SevenZip;
    }
    if link_blob.contains(".zip") {
        return ArchiveFormat::Zip;
    }
    if link_blob.contains(".rar") {
        return ArchiveFormat::Rar;
    }
    if haystack.contains("iso") {
        ArchiveFormat::Iso
    } else {
        ArchiveFormat::Unknown
    }
}

fn looks_multipart(link_blob: &str, content: &str) -> bool {
    let content = content.to_ascii_lowercase();
    link_blob.contains(".part")
        || content.contains(".part")
        || content.contains(".r00")
        || content.contains(".7z.001")
        || content.contains(".zip.001")
}

pub fn infer_region(title: &str, class_list: Option<&[String]>) -> Option<String> {
    let haystack = format!(
        "{} {}",
        title.to_ascii_lowercase(),
        class_list
            .unwrap_or(&[])
            .iter()
            .map(|value| value.to_ascii_lowercase())
            .collect::<Vec<_>>()
            .join(" ")
    );
    if haystack.contains("region-free") || haystack.contains("region free") {
        return Some("Region Free".to_owned());
    }
    if haystack.contains("ntsc-j") || haystack.contains("ntscj") {
        return Some("NTSC-J".to_owned());
    }
    if haystack.contains("ntsc-u") || haystack.contains("ntscu") {
        return Some("NTSC-U".to_owned());
    }
    if haystack.contains("ntsc") {
        return Some("NTSC".to_owned());
    }
    if haystack.contains("pal") {
        return Some("PAL".to_owned());
    }
    None
}

fn infer_uploader(yoast: Option<&WpYoast>, author: Option<u64>, content: &str) -> String {
    if let Some(name) = yoast.and_then(|yoast| yoast.author.as_deref()) {
        let name = name.trim();
        if !name.is_empty() {
            return name.to_owned();
        }
    }
    if let Some(name) = thanked_uploader(content) {
        return name;
    }
    if author == Some(1) {
        "psp".to_owned()
    } else {
        "Unknown".to_owned()
    }
}

fn thanked_uploader(content: &str) -> Option<String> {
    let lower = content.to_ascii_lowercase();
    let index = lower.find("thank @")?;
    let rest = &content[index + 7..];
    let name: String = rest
        .chars()
        .take_while(|character| character.is_ascii_alphanumeric() || *character == '_')
        .collect();
    (!name.is_empty()).then_some(name)
}

fn infer_size(content: &str) -> Option<String> {
    let bytes = content.as_bytes();
    let lower = content.to_ascii_lowercase();
    for (index, rest) in lower.match_indices("gb").chain(lower.match_indices("mb")) {
        if index < 2 {
            continue;
        }
        let unit = if rest.starts_with("gb") { "GB" } else { "MB" };
        let mut start = index;
        while start > 0 {
            let previous = bytes[start - 1];
            if previous.is_ascii_digit() || previous == b'.' || previous == b' ' {
                start -= 1;
            } else {
                break;
            }
        }
        let number = content[start..index].trim().replace(' ', "");
        if !number.is_empty() && number.parse::<f64>().is_ok() {
            return Some(format!("{number} {unit}"));
        }
    }
    None
}

fn infer_parts(content: &str, title: &str) -> Option<u32> {
    let haystack = format!("{title} {content}").to_ascii_lowercase();
    if let Some(count) = haystack
        .split(|character: char| !character.is_ascii_alphanumeric())
        .collect::<Vec<_>>()
        .windows(2)
        .find_map(|pair| {
            (pair[1] == "parts" || pair[1] == "part")
                .then(|| pair[0].parse::<u32>().ok())
                .flatten()
                .filter(|value| *value > 1)
        })
    {
        return Some(count);
    }
    let mut highest = 0_u32;
    for token in ["part", ".r0", ".7z.", ".zip."] {
        let mut rest = haystack.as_str();
        while let Some(index) = rest.find(token) {
            let after = &rest[index + token.len()..];
            let digits: String = after
                .chars()
                .take_while(|character| character.is_ascii_digit())
                .collect();
            if let Ok(value) = digits.parse::<u32>() {
                highest = highest.max(value);
            }
            rest = after;
        }
    }
    (highest > 1).then_some(highest)
}

pub fn parse_download_options(html: &str) -> Vec<DownloadOption> {
    let normalized = html
        .replace("<br />", "\n")
        .replace("<br/>", "\n")
        .replace("<br>", "\n")
        .replace("&#8212;", "\n")
        .replace('—', "\n");
    let mut options = Vec::new();
    let mut fallback_password = None;
    for chunk in normalized.lines() {
        let download_anchors: Vec<(String, String)> = extract_anchors(chunk)
            .into_iter()
            .filter(|(text, url)| is_download_option(text, url))
            .collect();
        let password = extract_listed_password(&strip_html(chunk));
        if download_anchors.is_empty() {
            if fallback_password.is_none() {
                fallback_password = password;
            }
            continue;
        }
        let chunk_start = options.len();
        for (text, url) in download_anchors {
            if options
                .iter()
                .any(|option: &DownloadOption| option.urls.iter().any(|known| known == &url))
            {
                continue;
            }
            let label = host_label(&text, &url);
            if let Some(option) = options[chunk_start..]
                .iter_mut()
                .find(|option| option.label.eq_ignore_ascii_case(&label))
            {
                option.urls.push(url);
                continue;
            }
            options.push(DownloadOption {
                label,
                url: url.clone(),
                urls: vec![url],
                password: password.clone(),
            });
        }
    }
    if let Some(fallback) = fallback_password {
        for option in &mut options {
            if option.password.is_none() {
                option.password = Some(fallback.clone());
            }
        }
    }
    options
}

fn extract_anchors(html: &str) -> Vec<(String, String)> {
    let lower = html.to_ascii_lowercase();
    let mut anchors = Vec::new();
    let mut search = 0;
    while let Some(relative) = lower.get(search..).and_then(|rest| rest.find("<a ")) {
        let start = search + relative;
        let Some(tag_end) = html.get(start..).and_then(|rest| rest.find('>')) else {
            break;
        };
        let tag = &html[start..start + tag_end];
        let tag_lower = tag.to_ascii_lowercase();
        let Some(href_at) = tag_lower.find("href=") else {
            search = start + 2;
            continue;
        };
        let after = tag[href_at + 5..].trim_start();
        let url = if let Some(quote) = after
            .chars()
            .next()
            .filter(|character| matches!(character, '"' | '\''))
        {
            after[1..]
                .split(quote)
                .next()
                .unwrap_or_default()
                .to_owned()
        } else {
            after
                .split(|character: char| character.is_ascii_whitespace() || character == '>')
                .next()
                .unwrap_or_default()
                .to_owned()
        };
        let inner_start = start + tag_end + 1;
        let inner_end = lower
            .get(inner_start..)
            .and_then(|rest| rest.find("</a>"))
            .map(|index| inner_start + index)
            .unwrap_or(html.len());
        let text = strip_html(&html[inner_start..inner_end]);
        if looks_like_download_link(&url) {
            anchors.push((text, decode_entities(&url)));
        }
        search = inner_end.saturating_add(4);
    }
    anchors
}

fn is_download_option(text: &str, url: &str) -> bool {
    looks_like_download_link(url)
        && (!matches!(classify_url(url), UrlClass::Ignore) || host_label(text, url) != "Download")
}

fn host_label(text: &str, url: &str) -> String {
    const HOSTS: &[(&str, &str)] = &[
        ("1fichier", "1Fichier"),
        ("1file", "1File"),
        ("filecrypt", "1File"),
        ("mediafire", "MediaFire"),
        ("mega.nz", "Mega"),
        ("mega.co.nz", "Mega"),
        ("mega", "Mega"),
        ("ranoz", "Ranoz"),
        ("pixeldrain", "Pixeldrain"),
        ("gofile", "Gofile"),
        ("dropbox", "Dropbox"),
        ("google.com", "Google Drive"),
        ("workupload", "Workupload"),
        ("rapidgator", "Rapidgator"),
        ("nitroflare", "Nitroflare"),
    ];
    let text_lower = text.to_ascii_lowercase();
    for (needle, label) in HOSTS {
        if text_lower.contains(needle) {
            return (*label).to_owned();
        }
    }
    let url_lower = url.to_ascii_lowercase();
    for (needle, label) in HOSTS {
        if url_lower.contains(needle) {
            return (*label).to_owned();
        }
    }
    let text = text.trim();
    if !text.is_empty() {
        return text.to_owned();
    }
    "Download".to_owned()
}

fn extract_listed_password(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let index = lower.find("password")?;
    let mut rest = text.get(index + 8..)?.trim_start();
    rest = rest.strip_prefix([':', '：']).unwrap_or(rest).trim_start();
    rest = rest.trim_start_matches(['-', '–', '—', ' ']);
    let value: String = rest
        .chars()
        .take_while(|character| {
            character.is_ascii_alphanumeric() || matches!(*character, '.' | '_' | '-' | '@')
        })
        .collect();
    let value = value.trim_matches('.').trim().to_owned();
    (value.len() >= 2).then_some(value)
}

pub fn content_mentions_password(content: &str) -> bool {
    let lower = content.to_ascii_lowercase();
    lower.contains("password") || lower.contains("passwd")
}

pub fn extract_hrefs(html: &str) -> Vec<String> {
    let lower = html.to_ascii_lowercase();
    let mut links = Vec::new();
    let mut rest = lower.as_str();
    let original = html;
    let mut offset = 0;
    while let Some(index) = rest.find("href=") {
        let absolute = offset + index + 5;
        let after = original.get(absolute..).unwrap_or("");
        let after = after.trim_start();
        let quote = after.chars().next();
        let url = if quote == Some('"') || quote == Some('\'') {
            let quote = quote.unwrap();
            after[1..]
                .split(quote)
                .next()
                .unwrap_or_default()
                .to_owned()
        } else {
            after
                .split(|character: char| character.is_ascii_whitespace() || character == '>')
                .next()
                .unwrap_or_default()
                .to_owned()
        };
        if looks_like_download_link(&url) {
            links.push(decode_entities(&url));
        }
        offset = absolute + 1;
        rest = &lower[offset.min(lower.len())..];
    }
    links.sort();
    links.dedup();
    links
}

fn looks_like_download_link(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("mailto:") || lower.contains("youtube.com") || lower.contains("youtu.be") {
        return false;
    }
    if lower.contains("blogspot.com") || lower.contains("adskeeper") || lower.contains("gamepciso")
    {
        return false;
    }
    lower.starts_with("http://") || lower.starts_with("https://")
}

pub fn classify_links(links: &[String]) -> (DownloadAccess, Vec<String>) {
    let mut direct = Vec::new();
    let mut hosted = false;
    for link in links {
        match classify_url(link) {
            UrlClass::Direct => direct.push(link.clone()),
            UrlClass::Hosted => hosted = true,
            UrlClass::Ignore => {}
        }
    }
    if !direct.is_empty() && !hosted {
        (DownloadAccess::Direct, direct)
    } else {
        (DownloadAccess::BrowserAuthentication, direct)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UrlClass {
    Direct,
    Hosted,
    Ignore,
}

pub(crate) fn classify_url(url: &str) -> UrlClass {
    let lower = url.to_ascii_lowercase();
    const HOSTED: &[&str] = &[
        "1fichier.com",
        "mediafire.com",
        "mega.nz",
        "mega.co.nz",
        "filecrypt.cc",
        "ranoz.gg",
        "drive.google.com",
        "docs.google.com",
        "dropbox.com",
        "pixeldrain.com",
        "gofile.io",
        "anonfiles",
        "workupload.com",
        "rapidgator.net",
        "uploaded.net",
        "nitroflare.com",
    ];
    if HOSTED.iter().any(|host| lower.contains(host)) {
        return UrlClass::Hosted;
    }
    let path = lower.split('?').next().unwrap_or(&lower);
    if path.ends_with(".iso")
        || path.ends_with(".zip")
        || path.ends_with(".rar")
        || path.ends_with(".7z")
        || path.ends_with(".001")
        || path.contains(".part") && (path.ends_with(".rar") || path.ends_with(".zip"))
    {
        return UrlClass::Direct;
    }
    UrlClass::Ignore
}

pub fn strip_html(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut in_tag = false;
    for character in input.chars() {
        match character {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => output.push(character),
            _ => {}
        }
    }
    decode_entities(&output)
        .replace("\\/", "/")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn decode_entities(input: &str) -> String {
    input
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#8212;", "—")
        .replace("&#8211;", "-")
        .replace("&#038;", "&")
        .replace("&nbsp;", " ")
        .replace("\\/", "/")
}

pub fn percent_encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(*byte as char);
            }
            b' ' => encoded.push('+'),
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

pub fn format_listing(index: usize, listing: &StoreListing) -> Vec<String> {
    let mut lines = vec![
        format!("{}. {}", index + 1, listing.title),
        format!("     Format: {}", listing.format.label()),
    ];
    if !listing.uploader.is_empty() {
        lines.push(format!("     Uploader: {}", listing.uploader));
    }
    if let Some(size) = &listing.size {
        lines.push(format!("     Size: {size}"));
    }
    if let Some(region) = &listing.region {
        lines.push(format!("     Region: {region}"));
    }
    if let Some(parts) = listing.parts {
        lines.push(format!("     Parts: {parts}"));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excludes_only_onefile_from_host_choices() {
        let option = |label: &str, url: &str| DownloadOption {
            label: label.into(),
            url: url.into(),
            urls: vec![url.into()],
            password: None,
        };
        assert!(is_onefile_option(&option(
            "1File",
            "https://short.example/abc"
        )));
        assert!(is_onefile_option(&option(
            "1Fichier",
            "https://1fichier.com/?abc"
        )));
        assert!(is_onefile_option(&option(
            "Download",
            "https://1fichier.com/?abc"
        )));
        for (label, url) in [
            ("Mega", "https://goo.gl/U67SV4"),
            ("MediaFire", "https://mediafire.com/file/missing"),
            ("Other host", "https://example.com/login-required"),
            ("Download", "https://example.com/1fichier.com/file"),
        ] {
            assert!(!is_onefile_option(&option(label, url)), "{label}");
        }
    }

    #[test]
    fn prepared_store_copy_replaces_iso_but_preserves_fallbacks() {
        let titles = [
            "Example Game [Region Free][ISO]",
            "Example Game [Jtag/RGH]",
            "Example Game 2 [ISO]",
            "Example Game [Limited Edition][ISO]",
            "Other Game [ISO]",
            "Other Game [ZIP]",
        ];
        let mut listings = titles
            .iter()
            .enumerate()
            .map(|(id, title)| {
                listing_from_catalog(catalog::CatalogEntry {
                    id: id as u64,
                    title: (*title).to_owned(),
                    page_url: format!("https://example.test/{id}"),
                })
            })
            .collect::<Vec<_>>();
        // A prepared release remains preferred even when distributed in parts.
        listings[1].format = ArchiveFormat::MultipartRar;
        prefer_prepared_listings(&mut listings);
        assert_eq!(
            listings
                .iter()
                .map(|listing| listing.title.as_str())
                .collect::<Vec<_>>(),
            titles[1..]
        );
    }

    #[test]
    fn store_title_key_preserves_editions_and_numbers() {
        assert_eq!(
            game_title_key("Example 2 [PAL][ISO]"),
            game_title_key("Example 2 JTAG/RGH")
        );
        assert_ne!(
            game_title_key("Example 2 [ISO]"),
            game_title_key("Example 3 [JTAG/RGH]")
        );
        assert_ne!(
            game_title_key("Example [Limited Edition][ISO]"),
            game_title_key("Example [JTAG/RGH]")
        );
    }

    #[test]
    fn parses_title_metadata_without_capturing_passwords() {
        let html = r#"
            <p>NAME Example Game FORMAT [Region Free][PAL][ISO]</p>
            <p>Link Download : <a href="https://1fichier.com/?abc">1File</a></p>
            <p>Thank @ExampleUploader</p>
            <p>Size 7.4 GB in 8 parts: game.part01.rar</p>
            <p>Password : SHOULD-NOT-BE-CAPTURED</p>
        "#;
        let links = extract_hrefs(html);
        assert_eq!(links, vec!["https://1fichier.com/?abc".to_owned()]);
        assert_eq!(
            infer_format("Example Game [Region Free][PAL][ISO]", None, &links, html),
            ArchiveFormat::Iso
        );
        assert_eq!(
            infer_region("Example Game [Region Free][PAL][ISO]", None).as_deref(),
            Some("Region Free")
        );
        assert_eq!(infer_size(html).as_deref(), Some("7.4 GB"));
        assert_eq!(infer_parts(html, "Example Game"), Some(8));
        assert!(content_mentions_password(html));
        assert_eq!(infer_uploader(None, None, html), "ExampleUploader");
        assert!(!infer_uploader(None, None, html).contains("SHOULD-NOT-BE-CAPTURED"));
        let (access, _) = classify_links(&links);
        assert_eq!(access, DownloadAccess::BrowserAuthentication);
        let options = parse_download_options(html);
        assert_eq!(options.len(), 1);
        assert_eq!(options[0].label, "1File");
        assert_eq!(
            options[0].password.as_deref(),
            Some("SHOULD-NOT-BE-CAPTURED")
        );
    }

    #[test]
    fn classifies_direct_archive_urls_separately_from_hosted_pages() {
        assert!(matches!(
            classify_url("https://cdn.example.com/game.part01.rar"),
            UrlClass::Direct
        ));
        assert!(matches!(
            classify_url("https://1fichier.com/?token"),
            UrlClass::Hosted
        ));
        let (access, direct) = classify_links(&[
            "https://cdn.example.com/game.zip".to_owned(),
            "https://cdn.example.com/readme.txt".to_owned(),
        ]);
        assert_eq!(access, DownloadAccess::Direct);
        assert_eq!(direct.len(), 1);
    }

    #[test]
    fn formats_search_result_lines() {
        let listing = StoreListing {
            id: "1".to_owned(),
            title: "Example Game".to_owned(),
            format: ArchiveFormat::MultipartRar,
            uploader: "ExampleUploader".to_owned(),
            size: Some("7.4 GB".to_owned()),
            region: Some("PAL".to_owned()),
            parts: Some(8),
            page_url: "https://example.com/game".to_owned(),
            password_required: true,
            access: DownloadAccess::BrowserAuthentication,
            direct_urls: Vec::new(),
            download_options: Vec::new(),
        };
        let lines = format_listing(0, &listing);
        assert_eq!(lines[0], "1. Example Game");
        assert_eq!(lines[1], "     Format: Multipart RAR");
        assert_eq!(lines[2], "     Uploader: ExampleUploader");
        assert_eq!(lines[3], "     Size: 7.4 GB");
        assert_eq!(lines[4], "     Region: PAL");
        assert_eq!(lines[5], "     Parts: 8");
    }

    #[test]
    fn short_queries_match_every_title_in_that_series() {
        assert!(title_matches_query(
            "Forza Motorsport 3 Limited Edition [PAL][ISO]",
            "Forza"
        ));
        assert!(title_matches_query(
            "Forza Horizon 2 [Jtag/RGH + DLC]",
            "for"
        ));
        assert!(title_matches_query(
            "Forza Motorsport 2 [PAL][ISO]",
            "forza 2"
        ));
        assert!(title_matches_query(
            "Forza Horizon 2 [Region Free][ISO]",
            "forza 2"
        ));
        assert!(title_matches_query("Halo Reach [Region Free][ISO]", "Halo"));
        assert!(title_matches_query("FIFA 19 [Jtag/RGH]", "fifa"));
        assert!(title_matches_query(
            "Call of Duty Modern Warfare 2 [ISO]",
            "cod"
        ));
        assert!(title_matches_query(
            "Grand Theft Auto V [Region Free][ISO]",
            "GTA"
        ));
        assert!(title_matches_query(
            "Grand Theft Auto IV Episodes From Liberty City",
            "gta 4"
        ));
        assert!(!title_matches_query("Destiny [Region Free][ISO]", "Halo"));
        assert!(!title_matches_query("NBA 2K17 [Jtag/RGH]", "GTA"));
        assert!(!title_matches_query(
            "Just Dance 2019 [Region Free][ISO]",
            "forza"
        ));
        assert_eq!(
            compare_search_relevance("NBA 2K17", "Grand Theft Auto V", "GTA"),
            Ordering::Greater
        );
    }

    #[test]
    fn pairs_each_download_host_with_the_password_on_that_option() {
        let html = r#"
            <p><strong>Download</strong><br />
            Game [Jtag/RGH] : <a href="https://goo.gl/tZM8GP">Mega</a> &#8211; Password : Newman<br />
            Game [Jtag/RGH] : <a href="https://www.mediafire.com/file/abc/game.rar/file">Mediafire</a> &#8211; <a href="https://filecrypt.cc/Container/E35ED7C7AF.html">1File</a> &#8211; Password : AnDreX<br />
            Password : DLXBGAME.COM</p>
        "#;
        let options = parse_download_options(html);
        assert_eq!(options.len(), 3);
        assert_eq!(options[0].label, "Mega");
        assert_eq!(options[0].url, "https://goo.gl/tZM8GP");
        assert_eq!(options[0].password.as_deref(), Some("Newman"));
        assert_eq!(options[1].label, "MediaFire");
        assert_eq!(options[1].password.as_deref(), Some("AnDreX"));
        assert_eq!(options[2].label, "1File");
        assert_eq!(options[2].password.as_deref(), Some("AnDreX"));
        assert!(
            options
                .iter()
                .all(|option| option.password.as_deref() != Some("DLXBGAME.COM"))
        );
    }

    #[test]
    fn groups_same_host_part_links_into_one_download_option() {
        let html = r#"Game : <a href="https://cdn.example/game.part1.rar">Direct</a> - <a href="https://cdn.example/game.part2.rar">Direct</a> - Password : example"#;
        let options = parse_download_options(html);
        assert_eq!(options.len(), 1);
        assert_eq!(options[0].urls.len(), 2);
        assert!(options[0].urls[0].ends_with("part1.rar"));
        assert!(options[0].urls[1].ends_with("part2.rar"));
    }
}
