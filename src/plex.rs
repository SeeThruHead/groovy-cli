use anyhow::{Context, Result};
use quick_xml::events::Event;
use quick_xml::Reader;

pub struct PlexClient {
    base_url: String,
    token: String,
}

#[derive(Debug, Clone)]
pub struct Library {
    pub key: String,
    pub title: String,
    pub lib_type: String, // "show", "movie", "artist"
}

#[derive(Debug, Clone)]
pub struct Show {
    pub title: String,
    pub rating_key: u64,
    pub library: String, // which library it came from
}

#[derive(Debug, Clone)]
pub struct Episode {
    pub title: String,
    pub rating_key: u64,
    pub season: u32,
    pub episode: u32,
    pub view_count: u32,
    pub show_title: String,
}

#[derive(Debug)]
pub struct MediaInfo {
    pub direct_play_url: String,
    pub title: String,
    pub duration_ms: u64,
    pub view_offset_ms: u64,
    pub audio_stream_index: Option<u32>,
    pub subtitle_stream_index: Option<u32>,
    pub subtitle_codec: Option<String>,
}

#[derive(Debug, Clone)]
pub struct OnDeckItem {
    pub title: String,
    pub show_title: String,
    pub rating_key: u64,
    pub season: u32,
    pub episode: u32,
    pub view_offset: u64, // ms into the episode
    pub item_type: String, // "episode", "movie"
}

impl PlexClient {
    pub fn new(server: &str, port: u16, token: &str) -> Self {
        Self {
            base_url: format!("http://{}:{}", server, port),
            token: token.to_string(),
        }
    }

    fn get(&self, path: &str) -> Result<String> {
        let sep = if path.contains('?') { '&' } else { '?' };
        let url = format!("{}{}{}X-Plex-Token={}", self.base_url, path, sep, self.token);
        let resp = reqwest::blocking::Client::new()
            .get(&url)
            .header("Accept", "application/xml")
            .send()
            .context("HTTP request failed")?;
        Ok(resp.text()?)
    }

    /// Discover all libraries on the server
    pub fn libraries(&self) -> Result<Vec<Library>> {
        let xml = self.get("/library/sections")?;
        Ok(parse_libraries(&xml))
    }

    /// Search across all video libraries (show + movie) for a title
    pub fn search(&self, query: &str) -> Result<Vec<Show>> {
        let libs = self.libraries()?;
        let mut results = vec![];
        for lib in &libs {
            match lib.lib_type.as_str() {
                "show" | "movie" => {
                    let xml = self.get(&format!(
                        "/library/sections/{}/all?title={}",
                        lib.key, query
                    ))?;
                    let mut shows = parse_shows(&xml);
                    for s in &mut shows {
                        s.library = lib.title.clone();
                    }
                    results.extend(shows);
                }
                _ => {} // skip music, photo, etc.
            }
        }
        Ok(results)
    }

    /// Get all episodes of a show
    pub fn get_episodes(&self, show_key: u64) -> Result<Vec<Episode>> {
        let xml = self.get(&format!("/library/metadata/{}/allLeaves", show_key))?;
        Ok(parse_episodes(&xml))
    }

    /// Get "On Deck" items (continue watching)
    pub fn on_deck(&self) -> Result<Vec<OnDeckItem>> {
        let xml = self.get("/library/onDeck")?;
        Ok(parse_on_deck(&xml))
    }

    /// Get recently watched items
    pub fn recently_watched(&self) -> Result<Vec<OnDeckItem>> {
        // Get recently viewed episodes/movies with viewCount > 0
        let xml = self.get("/library/recentlyViewed")?;
        Ok(parse_on_deck(&xml))
    }

    /// Report playback progress to Plex (updates On Deck, watch state)
    pub fn report_progress(&self, rating_key: u64, time_ms: u64, state: &str, duration_ms: u64) -> Result<()> {
        let path = format!(
            "/:/timeline?ratingKey={}&key=%2Flibrary%2Fmetadata%2F{}&state={}&time={}&duration={}&X-Plex-Token={}",
            rating_key, rating_key, state, time_ms, duration_ms, self.token
        );
        let url = format!("{}{}", self.base_url, path);
        let _ = reqwest::blocking::Client::new()
            .get(&url)
            .header("X-Plex-Client-Identifier", "groovy-cli")
            .header("X-Plex-Product", "Groovy CLI")
            .send();
        Ok(())
    }

    /// Mark item as fully watched
    pub fn scrobble(&self, rating_key: u64) -> Result<()> {
        let path = format!("/:/scrobble?identifier=com.plexapp.plugins.library&key={}", rating_key);
        let _ = self.get(&path)?;
        Ok(())
    }

    /// Resolve a rating key to a direct-play URL with subtitle/audio info
    pub fn resolve_media(&self, rating_key: u64, audio_lang: Option<&str>, sub_lang: Option<&str>) -> Result<MediaInfo> {
        let xml = self.get(&format!("/library/metadata/{}", rating_key))?;
        parse_media_info(&xml, &self.base_url, &self.token, audio_lang, sub_lang)
    }
}

/// Decode XML/HTML entities: &amp; &lt; &gt; &quot; &#39; &#NNN; &#xHH;
fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '&' {
            let mut entity = String::new();
            for ec in chars.by_ref() {
                if ec == ';' {
                    break;
                }
                entity.push(ec);
            }
            match entity.as_str() {
                "amp" => out.push('&'),
                "lt" => out.push('<'),
                "gt" => out.push('>'),
                "quot" => out.push('"'),
                "apos" => out.push('\''),
                s if s.starts_with('#') => {
                    let num = &s[1..];
                    let code = if let Some(hex) = num.strip_prefix('x') {
                        u32::from_str_radix(hex, 16).ok()
                    } else {
                        num.parse::<u32>().ok()
                    };
                    if let Some(ch) = code.and_then(char::from_u32) {
                        out.push(ch);
                    }
                }
                _ => {
                    out.push('&');
                    out.push_str(&entity);
                    out.push(';');
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn attr_str(e: &quick_xml::events::BytesStart, key: &[u8]) -> String {
    e.attributes()
        .flatten()
        .find(|a| a.key.as_ref() == key)
        .map(|a| decode_entities(&String::from_utf8_lossy(&a.value)))
        .unwrap_or_default()
}

fn attr_u64(e: &quick_xml::events::BytesStart, key: &[u8]) -> u64 {
    attr_str(e, key).parse().unwrap_or(0)
}

fn attr_u32(e: &quick_xml::events::BytesStart, key: &[u8]) -> u32 {
    attr_str(e, key).parse().unwrap_or(0)
}

fn parse_libraries(xml: &str) -> Vec<Library> {
    let mut reader = Reader::from_str(xml);
    let mut libs = vec![];
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Empty(ref e)) | Ok(Event::Start(ref e))
                if e.name().as_ref() == b"Directory" =>
            {
                let key = attr_str(e, b"key");
                let title = attr_str(e, b"title");
                let lib_type = attr_str(e, b"type");
                if !key.is_empty() {
                    libs.push(Library { key, title, lib_type });
                }
            }
            Ok(Event::Eof) => break,
            _ => {}
        }
        buf.clear();
    }
    libs
}

fn parse_shows(xml: &str) -> Vec<Show> {
    let mut reader = Reader::from_str(xml);
    let mut shows = vec![];
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Empty(ref e)) | Ok(Event::Start(ref e))
                if e.name().as_ref() == b"Directory" || e.name().as_ref() == b"Video" =>
            {
                let key = attr_u64(e, b"ratingKey");
                if key > 0 {
                    shows.push(Show {
                        title: attr_str(e, b"title"),
                        rating_key: key,
                        library: String::new(),
                    });
                }
            }
            Ok(Event::Eof) => break,
            _ => {}
        }
        buf.clear();
    }
    shows
}

fn parse_episodes(xml: &str) -> Vec<Episode> {
    let mut reader = Reader::from_str(xml);
    let mut episodes = vec![];
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Empty(ref e)) | Ok(Event::Start(ref e))
                if e.name().as_ref() == b"Video" =>
            {
                let key = attr_u64(e, b"ratingKey");
                if key > 0 {
                    episodes.push(Episode {
                        title: attr_str(e, b"title"),
                        rating_key: key,
                        season: attr_u32(e, b"parentIndex"),
                        episode: attr_u32(e, b"index"),
                        view_count: attr_u32(e, b"viewCount"),
                        show_title: attr_str(e, b"grandparentTitle"),
                    });
                }
            }
            Ok(Event::Eof) => break,
            _ => {}
        }
        buf.clear();
    }
    episodes
}

fn parse_on_deck(xml: &str) -> Vec<OnDeckItem> {
    let mut reader = Reader::from_str(xml);
    let mut items = vec![];
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Empty(ref e)) | Ok(Event::Start(ref e))
                if e.name().as_ref() == b"Video" =>
            {
                let key = attr_u64(e, b"ratingKey");
                let item_type = attr_str(e, b"type");
                if key > 0 {
                    items.push(OnDeckItem {
                        title: attr_str(e, b"title"),
                        show_title: attr_str(e, b"grandparentTitle"),
                        rating_key: key,
                        season: attr_u32(e, b"parentIndex"),
                        episode: attr_u32(e, b"index"),
                        view_offset: attr_u64(e, b"viewOffset"),
                        item_type,
                    });
                }
            }
            Ok(Event::Eof) => break,
            _ => {}
        }
        buf.clear();
    }
    items
}

fn parse_media_info(xml: &str, base_url: &str, token: &str, audio_lang: Option<&str>, sub_lang: Option<&str>) -> Result<MediaInfo> {
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut title = String::new();
    let mut duration_ms: u64 = 0;
    let mut view_offset_ms: u64 = 0;
    let mut part_key: Option<String> = None;
    let mut found_part = false;
    let mut audio_streams: Vec<(u32, String, bool)> = vec![]; // (index, lang, selected)
    let mut sub_streams: Vec<(u32, String, String, bool)> = vec![]; // (index, lang, codec, selected)

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Empty(ref e)) | Ok(Event::Start(ref e)) => {
                match e.name().as_ref() {
                    b"Video" => {
                        title = attr_str(e, b"title");
                        duration_ms = attr_u64(e, b"duration");
                        view_offset_ms = attr_u64(e, b"viewOffset");
                    }
                    b"Part" if part_key.is_none() => {
                        let k = attr_str(e, b"key");
                        if !k.is_empty() {
                            part_key = Some(k);
                            found_part = true;
                        }
                    }
                    b"Stream" if found_part => {
                        let stream_type = attr_str(e, b"streamType");
                        let index = attr_u32(e, b"index");
                        let lang = attr_str(e, b"languageCode");
                        let selected = attr_str(e, b"selected") == "1";
                        if stream_type == "2" {
                            audio_streams.push((index, lang, selected));
                        } else if stream_type == "3" {
                            let codec = attr_str(e, b"codec");
                            sub_streams.push((index, lang, codec, selected));
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            _ => {}
        }
        buf.clear();
    }

    // Pick subtitle: CLI flag > selected > English > first. "none" disables.
    let picked = if let Some(lang) = sub_lang {
        if lang.eq_ignore_ascii_case("none") || lang == "off" || lang == "0" {
            None
        } else {
            let lang_lower = lang.to_lowercase();
            sub_streams.iter().find(|s| {
                s.1.to_lowercase().starts_with(&lang_lower) ||
                lang_lower.starts_with(&s.1.to_lowercase())
            }).or_else(|| sub_streams.iter().find(|s| s.3))
              .or_else(|| sub_streams.first())
        }
    } else {
        sub_streams.iter().find(|s| s.3)
            .or_else(|| sub_streams.iter().find(|s| s.1.starts_with("eng")))
            .or_else(|| sub_streams.first())
    };

    let (subtitle_stream_index, subtitle_codec) = if let Some((idx, _, codec, _)) = picked {
        (Some(*idx), Some(codec.clone()))
    } else {
        (None, None)
    };

    let pk = part_key.context("No playable media part found")?;
    let direct_play_url = format!("{}{}?X-Plex-Token={}", base_url, pk, token);

    // Pick audio: CLI language flag > Plex selected > first
    let audio_stream_index = if let Some(lang) = audio_lang {
        let lang_lower = lang.to_lowercase();
        audio_streams.iter().find(|s| {
            s.1.to_lowercase().starts_with(&lang_lower) ||
            lang_lower.starts_with(&s.1.to_lowercase())
        }).or_else(|| audio_streams.iter().find(|s| s.2))
          .or_else(|| audio_streams.first())
          .map(|s| s.0)
    } else {
        audio_streams.iter().find(|s| s.2)
            .or_else(|| audio_streams.first())
            .map(|s| s.0)
    };

    Ok(MediaInfo {
        direct_play_url,
        title,
        duration_ms,
        view_offset_ms,
        audio_stream_index,
        subtitle_stream_index,
        subtitle_codec,
    })
}
