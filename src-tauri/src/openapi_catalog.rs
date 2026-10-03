//! Official `https://openapi.tidal.com/v2` JSON:API catalog.
//!
//! Playback stays on the private stream endpoints. `GET /trackManifests/{id}`
//! and `GET /videoManifests/{id}` are not used: third-party playback of full
//! tracks is not what those manifest routes are for in this client.
//!
//! Reads and writes try this module first. A 403, a shape the UI cannot
//! render, or a transport failure falls back to the private API. A 429, a
//! missing token, or a blocked proxy does not.

use super::{
    MediaMetadata, MixPageResult, PaginatedTracks, TidalAlbum, TidalAlbumDetail, TidalArtist,
    TidalArtistDetail, TidalPlaylist, TidalPlaylistCreator, TidalSearchResults, TidalTrack,
    TidalVideo, TIDAL_CLIENT_VERSION, TIDAL_OPENAPI_URL,
};
use crate::tidal_api::{DirectHitItem, SuggestionTextItem, SuggestionsResponse};
use crate::SoneError;
use serde_json::{json, Value};

const MAX_RELATION_PAGES: usize = 40;
const MAX_SEARCH_PAGES: usize = 8;
const MAX_MIX_PAGES: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CollectionKind {
    Tracks,
    Albums,
    Artists,
    Playlists,
    Videos,
}

impl CollectionKind {
    fn resource(self) -> &'static str {
        match self {
            Self::Tracks => "userCollectionTracks",
            Self::Albums => "userCollectionAlbums",
            Self::Artists => "userCollectionArtists",
            Self::Playlists => "userCollectionPlaylists",
            Self::Videos => "userCollectionVideos",
        }
    }

    fn item_type(self) -> &'static str {
        match self {
            Self::Tracks => "tracks",
            Self::Albums => "albums",
            Self::Artists => "artists",
            Self::Playlists => "playlists",
            Self::Videos => "videos",
        }
    }
}

#[derive(Debug, Clone)]
pub struct PlaylistItemRef {
    pub item_id: String,
    pub resource_id: String,
    pub resource_type: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PlaylistSort {
    Default,
    Field(&'static str, bool),
    Unsupported,
}

struct CatalogMap<T> {
    value: T,
    complete: bool,
}

struct Ident {
    id: String,
    typ: String,
    item_id: Option<String>,
    added_at: Option<String>,
}

struct MemberWalk {
    ids: Vec<String>,
    truncated: bool,
}

const USER_MIXES: &[(&str, &str, &str)] = &[
    ("userDailyMixes", "Daily Mix", "DAILY_MIX"),
    ("userDiscoveryMixes", "Discovery Mix", "DISCOVERY_MIX"),
    ("userNewReleaseMixes", "New Releases", "NEW_RELEASE_MIX"),
];

pub(super) fn fallback_after(err: &SoneError) -> bool {
    !matches!(
        err,
        SoneError::NotAuthenticated
            | SoneError::NotConfigured(_)
            | SoneError::ProxyBlocked { .. }
            | SoneError::Api { status: 429, .. }
    )
}

/// `Ok(Some)` keeps the official value. `Ok(None)` means the caller should use
/// the private API. Rate limits, missing credentials, and a blocked proxy
/// propagate.
pub(super) fn official_result<T>(
    label: &str,
    official: Result<T, SoneError>,
) -> Result<Option<T>, SoneError> {
    match official {
        Ok(value) => {
            log::debug!("[{label}] using official catalog");
            Ok(Some(value))
        }
        Err(err) if !fallback_after(&err) => Err(err),
        Err(err) => {
            log::warn!("[{label}] official catalog failed ({err}), using private api");
            Ok(None)
        }
    }
}

pub(super) fn playlist_sort(order: Option<&str>, direction: Option<&str>) -> PlaylistSort {
    let Some(order) = order.filter(|value| !value.is_empty()) else {
        return PlaylistSort::Default;
    };
    let field = match order {
        "ARTIST" => "artists.name",
        "ALBUM" => "albums.title",
        "DATE" => "addedAt",
        "TITLE" => "title",
        "DURATION" => "duration",
        "INDEX" => "itemIndex",
        _ => return PlaylistSort::Unsupported,
    };
    let desc = matches!(direction, Some("DESC" | "desc"));
    PlaylistSort::Field(field, desc)
}

pub(super) fn iso8601_duration_secs(raw: &str) -> Option<u32> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(number) = trimmed.parse::<f64>() {
        return finite_secs(number);
    }
    let upper = trimmed.to_ascii_uppercase();
    let mut rest = upper.strip_prefix('P')?;
    let mut total = 0.0;
    let mut consumed = false;
    if let Some((value, next)) = consume_unit(rest, 'W') {
        total += value * 604_800.0;
        rest = next;
        consumed = true;
    }
    if let Some((value, next)) = consume_unit(rest, 'D') {
        total += value * 86_400.0;
        rest = next;
        consumed = true;
    }
    if let Some(time) = rest.strip_prefix('T') {
        rest = time;
        if let Some((value, next)) = consume_unit(rest, 'H') {
            total += value * 3_600.0;
            rest = next;
            consumed = true;
        }
        if let Some((value, next)) = consume_unit(rest, 'M') {
            total += value * 60.0;
            rest = next;
            consumed = true;
        }
        if let Some((value, next)) = consume_unit(rest, 'S') {
            total += value;
            rest = next;
            consumed = true;
        }
    }
    if !consumed || !rest.is_empty() {
        return None;
    }
    finite_secs(total)
}

pub(super) fn popularity_percent(value: f64) -> Option<u32> {
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    let scaled = if value <= 1.0 { value * 100.0 } else { value };
    if scaled > 100.0 {
        return None;
    }
    Some(scaled.round() as u32)
}

pub(super) fn next_cursor(node: &Value) -> Option<String> {
    let links = node.get("links")?;
    if let Some(cursor) = links
        .get("meta")
        .and_then(|meta| meta.get("nextCursor"))
        .and_then(Value::as_str)
        .filter(|cursor| !cursor.is_empty())
    {
        return Some(cursor.to_string());
    }
    let next = links.get("next").and_then(Value::as_str)?;
    cursor_from_url(next)
}

fn finite_secs(value: f64) -> Option<u32> {
    if !value.is_finite() || value < 0.0 || value > u32::MAX as f64 {
        return None;
    }
    Some(value.round() as u32)
}

fn consume_unit(input: &str, unit: char) -> Option<(f64, &str)> {
    let end = input
        .find(|ch: char| !(ch.is_ascii_digit() || ch == '.'))
        .unwrap_or(input.len());
    if end == 0 {
        return None;
    }
    let number: f64 = input[..end].parse().ok()?;
    let rest = input[end..].strip_prefix(unit)?;
    Some((number, rest))
}

fn cursor_from_url(url: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=')?;
        let key = key
            .replace("%5B", "[")
            .replace("%5b", "[")
            .replace("%5D", "]")
            .replace("%5d", "]");
        if key == "page[cursor]" {
            return percent_decode(&value.replace('+', " ")).filter(|cursor| !cursor.is_empty());
        }
    }
    None
}

fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn require_complete<T>(mapped: CatalogMap<T>, what: &str) -> Result<T, SoneError> {
    if mapped.complete {
        Ok(mapped.value)
    } else {
        Err(SoneError::Parse(format!(
            "official {what} missing display fields"
        )))
    }
}

fn included_slice(doc: &Value) -> &[Value] {
    doc.get("included")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn resource_data(doc: &Value) -> Result<&Value, SoneError> {
    let data = doc
        .get("data")
        .ok_or_else(|| SoneError::Parse("official document missing data".into()))?;
    if data.is_object() {
        Ok(data)
    } else {
        Err(SoneError::Parse(
            "official document data is not a resource".into(),
        ))
    }
}

fn ident_from(value: &Value) -> Option<Ident> {
    let id = value.get("id").and_then(Value::as_str)?.to_string();
    let typ = value.get("type").and_then(Value::as_str)?.to_string();
    if id.is_empty() || typ.is_empty() {
        return None;
    }
    let meta = value.get("meta");
    let item_id = meta
        .and_then(|item| item.get("itemId"))
        .and_then(Value::as_str)
        .filter(|item| !item.is_empty())
        .map(str::to_string);
    let added_at = meta
        .and_then(|item| item.get("addedAt"))
        .and_then(Value::as_str)
        .map(str::to_string);
    Some(Ident {
        id,
        typ,
        item_id,
        added_at,
    })
}

fn idents_of(node: &Value) -> Vec<Ident> {
    match node.get("data") {
        Some(Value::Array(items)) => items.iter().filter_map(ident_from).collect(),
        Some(Value::Object(_)) => node.get("data").and_then(ident_from).into_iter().collect(),
        _ => Vec::new(),
    }
}

fn idents_in_data(doc: &Value) -> Result<Vec<Ident>, SoneError> {
    match doc.get("data") {
        None => Err(SoneError::Parse("official document missing data".into())),
        Some(Value::Array(items)) => {
            let idents: Vec<Ident> = items.iter().filter_map(ident_from).collect();
            if idents.len() != items.len() {
                return Err(SoneError::Parse(
                    "official relationship identifier missing id or type".into(),
                ));
            }
            Ok(idents)
        }
        Some(Value::Null) => Ok(Vec::new()),
        Some(_) => Err(SoneError::Parse(
            "official relationship data is not a list".into(),
        )),
    }
}

fn rel_idents(resource: &Value, name: &str) -> Vec<Ident> {
    resource
        .get("relationships")
        .and_then(|relationships| relationships.get(name))
        .map(idents_of)
        .unwrap_or_default()
}

fn rel_cursor(resource: &Value, name: &str) -> Option<String> {
    resource
        .get("relationships")
        .and_then(|relationships| relationships.get(name))
        .and_then(next_cursor)
}

fn find_included<'a>(included: &'a [Value], typ: &str, id: &str) -> Option<&'a Value> {
    included.iter().find(|entry| {
        entry.get("type").and_then(Value::as_str) == Some(typ)
            && entry.get("id").and_then(Value::as_str) == Some(id)
    })
}

fn attr<'a>(resource: &'a Value, key: &str) -> Option<&'a Value> {
    resource.get("attributes")?.get(key)
}

fn attr_str<'a>(resource: &'a Value, key: &str) -> Option<&'a str> {
    attr(resource, key).and_then(Value::as_str)
}

fn attr_bool(resource: &Value, key: &str) -> Option<bool> {
    attr(resource, key).and_then(Value::as_bool)
}

fn attr_u32(resource: &Value, key: &str) -> Option<u32> {
    attr(resource, key)
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
}

fn attr_f64(resource: &Value, key: &str) -> Option<f64> {
    attr(resource, key).and_then(Value::as_f64)
}

fn copyright_text(resource: &Value) -> Option<String> {
    match attr(resource, "copyright")? {
        Value::String(text) => Some(text.clone()),
        Value::Object(map) => map.get("text").and_then(Value::as_str).map(str::to_string),
        _ => None,
    }
}

fn duration_secs(resource: &Value) -> Option<u32> {
    match attr(resource, "duration")? {
        Value::String(text) => iso8601_duration_secs(text),
        Value::Number(number) => number
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .or_else(|| number.as_f64().and_then(finite_secs)),
        _ => None,
    }
}

fn numeric_id(id: &str) -> Option<u64> {
    id.parse().ok()
}

fn art_id(resource: &Value, relationship: &str) -> Option<String> {
    rel_idents(resource, relationship)
        .into_iter()
        .find(|ident| !ident.id.is_empty())
        .map(|ident| ident.id)
}

fn media_tags(resource: &Value) -> Vec<String> {
    attr(resource, "mediaTags")
        .and_then(Value::as_array)
        .map(|tags| {
            tags.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn quality_tag(tags: &[String]) -> Option<String> {
    const ORDER: [&str; 4] = ["HIRES_LOSSLESS", "LOSSLESS", "HIGH", "LOW"];
    ORDER
        .iter()
        .find(|quality| tags.iter().any(|tag| tag == *quality))
        .map(|quality| (*quality).to_string())
        .or_else(|| tags.first().cloned())
}

pub(super) fn resource_image(uuid: &str, size: u32) -> String {
    if uuid.starts_with("http://") || uuid.starts_with("https://") {
        return uuid.to_string();
    }
    let path = uuid.replace('-', "/");
    format!("https://resources.tidal.com/images/{path}/{size}x{size}.jpg")
}

fn canonical_media_type(raw: &str) -> Result<&'static str, SoneError> {
    match raw {
        "track" | "tracks" => Ok("tracks"),
        "video" | "videos" => Ok("videos"),
        _ => Err(SoneError::Parse(format!(
            "unsupported playlist item type {raw}"
        ))),
    }
}

fn json_status(path: &str, status: reqwest::StatusCode, body: String) -> Result<Value, SoneError> {
    if !status.is_success() {
        if path.contains("/users/") {
            log::warn!("[openapi] {path} -> {status} body=<redacted>");
        } else {
            let preview: String = body.chars().take(200).collect();
            log::warn!("[openapi] {path} -> {status} {preview}");
        }
        return Err(SoneError::Api {
            status: status.as_u16(),
            body,
        });
    }
    if body.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&body).map_err(|err| SoneError::Parse(format!("openapi {path}: {err}")))
}

fn out_of_range() -> SoneError {
    SoneError::Api {
        status: 404,
        body: "official playlist index out of range".into(),
    }
}

fn track_shell(id: u64, title: String, duration: u32) -> TidalTrack {
    TidalTrack {
        id,
        title,
        duration,
        version: None,
        artist: None,
        artists: None,
        album: None,
        audio_quality: None,
        track_number: None,
        volume_number: None,
        date_added: None,
        isrc: None,
        explicit: None,
        popularity: None,
        replay_gain: None,
        peak: None,
        copyright: None,
        url: None,
        stream_ready: None,
        allow_streaming: None,
        premium_streaming_only: None,
        stream_start_date: None,
        audio_modes: None,
        media_metadata: None,
        mixes: None,
        item_type: Some("track".into()),
        image_id: None,
        playlist_item_id: None,
    }
}

fn album_shell(id: u64, title: String) -> TidalAlbumDetail {
    TidalAlbumDetail {
        id,
        title,
        version: None,
        cover: None,
        vibrant_color: None,
        video_cover: None,
        artist: None,
        artists: None,
        number_of_tracks: None,
        number_of_videos: None,
        number_of_volumes: None,
        duration: None,
        release_date: None,
        upc: None,
        album_type: None,
        copyright: None,
        explicit: None,
        popularity: None,
        url: None,
        audio_quality: None,
        stream_ready: None,
        allow_streaming: None,
        stream_start_date: None,
        audio_modes: None,
        media_metadata: None,
    }
}

fn artist_shell(id: u64, name: String) -> TidalArtist {
    TidalArtist {
        id,
        name,
        picture: None,
        artwork_id: None,
        selected_album_cover_fallback: None,
        artist_type: None,
        handle: None,
    }
}

fn video_shell(id: u64, title: String) -> TidalVideo {
    TidalVideo {
        id,
        title,
        duration: None,
        image_id: None,
        vibrant_color: None,
        quality: None,
        video_type: None,
        explicit: None,
        ads_pre_paywall_only: None,
        artist: None,
        artists: None,
    }
}

fn direct_hit(hit_type: &str) -> DirectHitItem {
    DirectHitItem {
        hit_type: hit_type.to_string(),
        id: None,
        uuid: None,
        name: None,
        title: None,
        picture: None,
        artwork_id: None,
        selected_album_cover_fallback: None,
        cover: None,
        image: None,
        artist_name: None,
        album_id: None,
        album_title: None,
        album_cover: None,
        duration: None,
        number_of_tracks: None,
        track: None,
        video: None,
    }
}

fn map_artist_resource(resource: &Value) -> Option<TidalArtist> {
    let id = numeric_id(resource.get("id").and_then(Value::as_str)?)?;
    let name = attr_str(resource, "name").unwrap_or("").to_string();
    if name.is_empty() {
        return None;
    }
    let picture = art_id(resource, "profileArt");
    let mut artist = artist_shell(id, name);
    artist.picture = picture.clone();
    artist.artwork_id = picture;
    artist.handle = attr_str(resource, "handle")
        .filter(|handle| !handle.is_empty())
        .map(str::to_string);
    Some(artist)
}

fn artist_named(artist: &TidalArtist) -> bool {
    !artist.name.is_empty()
}

fn artist_card_complete(artist: &TidalArtist) -> bool {
    artist_named(artist) && artist.picture.is_some()
}

fn map_artist_list(resource: &Value, included: &[Value]) -> Vec<TidalArtist> {
    rel_idents(resource, "artists")
        .into_iter()
        .filter_map(|ident| {
            let artist = find_included(included, &ident.typ, &ident.id)?;
            map_artist_resource(artist)
        })
        .collect()
}

fn map_album_resource(resource: &Value, included: &[Value]) -> Option<TidalAlbumDetail> {
    let id = numeric_id(resource.get("id").and_then(Value::as_str)?)?;
    let title = attr_str(resource, "title").unwrap_or("").to_string();
    if title.is_empty() {
        return None;
    }
    let artists = map_artist_list(resource, included);
    let tags = media_tags(resource);
    let mut album = album_shell(id, title);
    album.version = attr_str(resource, "version").map(str::to_string);
    album.cover = art_id(resource, "coverArt");
    album.artists = Some(artists);
    album.number_of_tracks = attr_u32(resource, "numberOfItems");
    album.number_of_volumes = attr_u32(resource, "numberOfVolumes");
    album.duration = duration_secs(resource);
    album.release_date = attr_str(resource, "releaseDate").map(str::to_string);
    album.upc = attr_str(resource, "barcodeId").map(str::to_string);
    album.album_type = attr_str(resource, "albumType")
        .or_else(|| attr_str(resource, "type"))
        .map(str::to_string);
    album.copyright = copyright_text(resource);
    album.explicit = attr_bool(resource, "explicit");
    album.popularity = attr_f64(resource, "popularity").and_then(popularity_percent);
    album.audio_quality = quality_tag(&tags);
    if !tags.is_empty() {
        album.media_metadata = Some(MediaMetadata { tags });
    }
    album.backfill_artist();
    Some(album)
}

fn album_detail_complete(album: &TidalAlbumDetail) -> bool {
    !album.title.is_empty()
        && album.cover.is_some()
        && album.artist.as_ref().is_some_and(artist_named)
}

fn to_tidal_album(detail: &TidalAlbumDetail) -> TidalAlbum {
    TidalAlbum {
        id: detail.id,
        title: detail.title.clone(),
        cover: detail.cover.clone(),
        vibrant_color: detail.vibrant_color.clone(),
        video_cover: detail.video_cover.clone(),
        release_date: detail.release_date.clone(),
    }
}

fn map_video_resource(resource: &Value, included: &[Value]) -> Option<TidalVideo> {
    let id = numeric_id(resource.get("id").and_then(Value::as_str)?)?;
    let title = attr_str(resource, "title").unwrap_or("").to_string();
    if title.is_empty() {
        return None;
    }
    let artists = map_artist_list(resource, included);
    let mut video = video_shell(id, title);
    video.duration = duration_secs(resource);
    video.image_id = art_id(resource, "thumbnailArt");
    video.explicit = attr_bool(resource, "explicit");
    video.artists = Some(artists);
    video.artist = video
        .artists
        .as_ref()
        .and_then(|artists| artists.first().cloned());
    Some(video)
}

fn video_complete(video: &TidalVideo) -> bool {
    !video.title.is_empty()
        && video.image_id.is_some()
        && video.artist.as_ref().is_some_and(artist_named)
}

fn map_track_resource(resource: &Value, included: &[Value], ident: &Ident) -> Option<TidalTrack> {
    if ident.typ == "videos" || resource.get("type").and_then(Value::as_str) == Some("videos") {
        let video = map_video_resource(resource, included)?;
        let mut track = track_shell(video.id, video.title.clone(), video.duration.unwrap_or(0));
        track.item_type = Some("video".into());
        track.image_id = video.image_id.clone();
        track.explicit = video.explicit;
        track.artist = video.artist.clone();
        track.artists = video.artists.clone();
        track.playlist_item_id = ident.item_id.clone();
        track.date_added = ident.added_at.clone();
        track.duration = video.duration.unwrap_or(0);
        return Some(track);
    }
    let id = numeric_id(resource.get("id").and_then(Value::as_str)?)?;
    let title = attr_str(resource, "title").unwrap_or("").to_string();
    if title.is_empty() {
        return None;
    }
    let artists = map_artist_list(resource, included);
    let album = rel_idents(resource, "albums")
        .into_iter()
        .find_map(|album_ident| {
            let album = find_included(included, &album_ident.typ, &album_ident.id)?;
            map_album_resource(album, included).map(|detail| to_tidal_album(&detail))
        });
    let tags = media_tags(resource);
    let mut track = track_shell(id, title, duration_secs(resource).unwrap_or(0));
    track.version = attr_str(resource, "version").map(str::to_string);
    track.artists = Some(artists);
    track.album = album;
    track.audio_quality = quality_tag(&tags);
    track.date_added = ident.added_at.clone();
    track.isrc = attr_str(resource, "isrc").map(str::to_string);
    track.explicit = attr_bool(resource, "explicit");
    track.popularity = attr_f64(resource, "popularity").and_then(popularity_percent);
    track.copyright = copyright_text(resource);
    track.item_type = Some("track".into());
    track.playlist_item_id = ident.item_id.clone();
    if !tags.is_empty() {
        track.media_metadata = Some(MediaMetadata { tags });
    }
    track.backfill_artist();
    Some(track)
}

fn media_complete(track: &TidalTrack) -> bool {
    if track.title.is_empty() || !track.artist.as_ref().is_some_and(artist_named) {
        return false;
    }
    if track.item_type.as_deref() == Some("video") {
        return track.image_id.is_some();
    }
    track
        .album
        .as_ref()
        .is_some_and(|album| !album.title.is_empty() && album.cover.is_some())
}

fn map_ident_slice(idents: &[Ident], included: &[Value]) -> CatalogMap<Vec<TidalTrack>> {
    let mut tracks = Vec::with_capacity(idents.len());
    let mut complete = true;
    for ident in idents {
        let Some(resource) = find_included(included, &ident.typ, &ident.id) else {
            complete = false;
            continue;
        };
        let Some(track) = map_track_resource(resource, included, ident) else {
            complete = false;
            continue;
        };
        if !media_complete(&track) {
            complete = false;
        }
        tracks.push(track);
    }
    if tracks.len() != idents.len() {
        complete = false;
    }
    CatalogMap {
        value: tracks,
        complete,
    }
}

fn map_media_document(doc: &Value) -> Result<CatalogMap<Vec<TidalTrack>>, SoneError> {
    let idents = idents_in_data(doc)?;
    Ok(map_ident_slice(&idents, included_slice(doc)))
}

fn map_album_idents(idents: &[Ident], included: &[Value]) -> CatalogMap<Vec<TidalAlbumDetail>> {
    let mut albums = Vec::with_capacity(idents.len());
    let mut complete = true;
    for ident in idents {
        let Some(resource) = find_included(included, &ident.typ, &ident.id) else {
            complete = false;
            continue;
        };
        let Some(album) = map_album_resource(resource, included) else {
            complete = false;
            continue;
        };
        if !album_detail_complete(&album) {
            complete = false;
        }
        albums.push(album);
    }
    if albums.len() != idents.len() {
        complete = false;
    }
    CatalogMap {
        value: albums,
        complete,
    }
}

fn map_album_document(doc: &Value) -> Result<CatalogMap<TidalAlbumDetail>, SoneError> {
    let data = resource_data(doc)?;
    let included = included_slice(doc);
    let album = map_album_resource(data, included)
        .ok_or_else(|| SoneError::Parse("official album missing id or title".into()))?;
    let complete = album_detail_complete(&album);
    Ok(CatalogMap {
        value: album,
        complete,
    })
}

fn map_artist_detail(doc: &Value) -> Result<CatalogMap<TidalArtistDetail>, SoneError> {
    let data = resource_data(doc)?;
    let artist = map_artist_resource(data)
        .ok_or_else(|| SoneError::Parse("official artist missing id or name".into()))?;
    let detail = TidalArtistDetail {
        id: artist.id,
        name: artist.name.clone(),
        picture: artist.picture.clone(),
        artwork_id: artist.artwork_id.clone(),
        selected_album_cover_fallback: None,
        handle: artist.handle.clone(),
        user_id: None,
        popularity: attr_f64(data, "popularity").and_then(popularity_percent),
        url: None,
        spotlighted: attr_bool(data, "spotlighted"),
        artist_types: None,
        artist_roles: None,
        mixes: None,
    };
    Ok(CatalogMap {
        value: detail,
        complete: artist_card_complete(&artist),
    })
}

fn map_video_document(doc: &Value) -> Result<CatalogMap<TidalVideo>, SoneError> {
    let data = resource_data(doc)?;
    let video = map_video_resource(data, included_slice(doc))
        .ok_or_else(|| SoneError::Parse("official video missing id or title".into()))?;
    let complete = video_complete(&video);
    Ok(CatalogMap {
        value: video,
        complete,
    })
}

fn map_playlist_resource(resource: &Value, included: &[Value]) -> Option<TidalPlaylist> {
    let uuid = resource.get("id").and_then(Value::as_str)?.to_string();
    let title = attr_str(resource, "name").unwrap_or("").to_string();
    if uuid.is_empty() || title.is_empty() {
        return None;
    }
    let cover = art_id(resource, "coverArt");
    let owner = first_numeric_owner(resource, included);
    Some(TidalPlaylist {
        uuid,
        title,
        description: attr_str(resource, "description").map(str::to_string),
        image: cover,
        number_of_tracks: attr_u32(resource, "numberOfTrackItems")
            .or_else(|| attr_u32(resource, "numberOfItems")),
        number_of_videos: attr_u32(resource, "numberOfVideoItems"),
        creator: owner.map(|(id, name)| TidalPlaylistCreator {
            id: Some(id),
            name: if name.is_empty() { None } else { Some(name) },
        }),
        playlist_type: attr_str(resource, "playlistType").map(str::to_string),
        duration: duration_secs(resource),
        last_updated: attr_str(resource, "lastModifiedAt").map(str::to_string),
        access_type: attr_str(resource, "accessType").map(str::to_string),
    })
}

fn playlist_card_complete(playlist: &TidalPlaylist) -> bool {
    !playlist.title.is_empty() && playlist.image.is_some()
}

fn first_numeric_owner(resource: &Value, included: &[Value]) -> Option<(u64, String)> {
    rel_idents(resource, "owners")
        .into_iter()
        .find_map(|ident| {
            let id = numeric_id(&ident.id)?;
            let name = find_included(included, &ident.typ, &ident.id)
                .and_then(|owner| {
                    attr_str(owner, "name")
                        .filter(|name| !name.is_empty())
                        .or_else(|| attr_str(owner, "username"))
                })
                .unwrap_or("")
                .to_string();
            Some((id, name))
        })
}

fn map_playlist_details(doc: &Value) -> Result<CatalogMap<Value>, SoneError> {
    let data = resource_data(doc)?;
    let playlist = map_playlist_resource(data, included_slice(doc))
        .ok_or_else(|| SoneError::Parse("official playlist missing id or name".into()))?;
    let duration = playlist.duration;
    let creator_id = playlist.creator.as_ref().and_then(|creator| creator.id);
    let complete = playlist_card_complete(&playlist) && creator_id.is_some() && duration.is_some();
    let value = json!({
        "uuid": playlist.uuid,
        "title": playlist.title,
        "description": playlist.description,
        "image": playlist.image,
        "squareImage": playlist.image,
        "numberOfTracks": playlist.number_of_tracks.unwrap_or(0),
        "numberOfVideos": playlist.number_of_videos.unwrap_or(0),
        "creator": {
            "id": creator_id.unwrap_or(0),
            "name": playlist.creator.as_ref().and_then(|creator| creator.name.clone()),
        },
        "type": playlist.playlist_type,
        "duration": duration.unwrap_or(0),
        "lastUpdated": playlist.last_updated,
        "accessType": playlist.access_type,
    });
    Ok(CatalogMap { value, complete })
}

fn hit_type_label(json_type: &str) -> Option<&'static str> {
    match json_type {
        "artists" => Some("ARTISTS"),
        "albums" => Some("ALBUMS"),
        "tracks" => Some("TRACKS"),
        "playlists" => Some("PLAYLISTS"),
        "videos" => Some("VIDEOS"),
        _ => None,
    }
}

fn direct_hit_from_ident(ident: &Ident, included: &[Value]) -> Option<DirectHitItem> {
    let label = hit_type_label(&ident.typ)?;
    let resource = find_included(included, &ident.typ, &ident.id)?;
    let mut hit = direct_hit(label);
    match ident.typ.as_str() {
        "artists" => {
            let artist = map_artist_resource(resource)?;
            if !artist_card_complete(&artist) {
                return None;
            }
            hit.id = Some(artist.id);
            hit.name = Some(artist.name);
            hit.picture = artist.picture.clone();
            hit.artwork_id = artist.artwork_id;
        }
        "albums" => {
            let album = map_album_resource(resource, included)?;
            if !album_detail_complete(&album) {
                return None;
            }
            hit.id = Some(album.id);
            hit.title = Some(album.title);
            hit.cover = album.cover.clone();
            hit.artist_name = album.artist.as_ref().map(|artist| artist.name.clone());
            hit.duration = album.duration;
            hit.number_of_tracks = album.number_of_tracks;
        }
        "tracks" => {
            let track = map_track_resource(resource, included, ident)?;
            if !media_complete(&track) {
                return None;
            }
            hit.id = Some(track.id);
            hit.title = Some(track.title.clone());
            hit.artist_name = track.artist.as_ref().map(|artist| artist.name.clone());
            hit.album_id = track.album.as_ref().map(|album| album.id);
            hit.album_title = track.album.as_ref().map(|album| album.title.clone());
            hit.album_cover = track.album.as_ref().and_then(|album| album.cover.clone());
            hit.duration = Some(track.duration);
            hit.track = Some(track);
        }
        "playlists" => {
            let playlist = map_playlist_resource(resource, included)?;
            if !playlist_card_complete(&playlist) {
                return None;
            }
            hit.uuid = Some(playlist.uuid);
            hit.title = Some(playlist.title);
            hit.image = playlist.image;
            hit.number_of_tracks = playlist.number_of_tracks;
            hit.duration = playlist.duration;
        }
        "videos" => {
            let video = map_video_resource(resource, included)?;
            if !video_complete(&video) {
                return None;
            }
            hit.id = Some(video.id);
            hit.title = Some(video.title.clone());
            hit.image = video.image_id.clone();
            hit.artist_name = video.artist.as_ref().map(|artist| artist.name.clone());
            hit.duration = video.duration;
            hit.video = Some(video);
        }
        _ => return None,
    }
    Some(hit)
}

fn map_direct_hits(idents: &[Ident], included: &[Value]) -> CatalogMap<Vec<DirectHitItem>> {
    let mut hits = Vec::new();
    let mut complete = true;
    for ident in idents {
        if hit_type_label(&ident.typ).is_none() {
            continue;
        }
        match direct_hit_from_ident(ident, included) {
            Some(hit) => hits.push(hit),
            None => complete = false,
        }
    }
    CatalogMap {
        value: hits,
        complete,
    }
}

fn map_artist_idents(idents: &[Ident], included: &[Value]) -> CatalogMap<Vec<TidalArtist>> {
    let mut artists = Vec::with_capacity(idents.len());
    let mut complete = true;
    for ident in idents {
        let Some(resource) = find_included(included, &ident.typ, &ident.id) else {
            complete = false;
            continue;
        };
        let Some(artist) = map_artist_resource(resource) else {
            complete = false;
            continue;
        };
        if !artist_card_complete(&artist) {
            complete = false;
        }
        artists.push(artist);
    }
    if artists.len() != idents.len() {
        complete = false;
    }
    CatalogMap {
        value: artists,
        complete,
    }
}

fn map_playlist_idents(idents: &[Ident], included: &[Value]) -> CatalogMap<Vec<TidalPlaylist>> {
    let mut playlists = Vec::with_capacity(idents.len());
    let mut complete = true;
    for ident in idents {
        let Some(resource) = find_included(included, &ident.typ, &ident.id) else {
            complete = false;
            continue;
        };
        let Some(playlist) = map_playlist_resource(resource, included) else {
            complete = false;
            continue;
        };
        if !playlist_card_complete(&playlist) {
            complete = false;
        }
        playlists.push(playlist);
    }
    if playlists.len() != idents.len() {
        complete = false;
    }
    CatalogMap {
        value: playlists,
        complete,
    }
}

fn map_video_idents(idents: &[Ident], included: &[Value]) -> CatalogMap<Vec<TidalVideo>> {
    let mut videos = Vec::with_capacity(idents.len());
    let mut complete = true;
    for ident in idents {
        let Some(resource) = find_included(included, &ident.typ, &ident.id) else {
            complete = false;
            continue;
        };
        let Some(video) = map_video_resource(resource, included) else {
            complete = false;
            continue;
        };
        if !video_complete(&video) {
            complete = false;
        }
        videos.push(video);
    }
    if videos.len() != idents.len() {
        complete = false;
    }
    CatalogMap {
        value: videos,
        complete,
    }
}

fn take_limited<T>(mapped: CatalogMap<Vec<T>>, limit: u32) -> CatalogMap<Vec<T>> {
    let mut value = mapped.value;
    let max = limit as usize;
    if value.len() > max {
        value.truncate(max);
    }
    CatalogMap {
        value,
        complete: mapped.complete,
    }
}

fn map_search_document(
    doc: &Value,
    limit: u32,
) -> Result<CatalogMap<TidalSearchResults>, SoneError> {
    let data = resource_data(doc)?;
    let included = included_slice(doc);
    let artists = take_limited(
        map_artist_idents(&rel_idents(data, "artists"), included),
        limit,
    );
    let albums = take_limited(
        map_album_idents(&rel_idents(data, "albums"), included),
        limit,
    );
    let tracks = take_limited(
        map_ident_slice(&rel_idents(data, "tracks"), included),
        limit,
    );
    let playlists = take_limited(
        map_playlist_idents(&rel_idents(data, "playlists"), included),
        limit,
    );
    let videos = take_limited(
        map_video_idents(&rel_idents(data, "videos"), included),
        limit,
    );
    let top_hits = take_limited(
        map_direct_hits(&rel_idents(data, "topHits"), included),
        limit,
    );
    let complete = artists.complete
        && albums.complete
        && tracks.complete
        && playlists.complete
        && videos.complete
        && top_hits.complete;
    Ok(CatalogMap {
        value: TidalSearchResults {
            artists: artists.value,
            albums: albums.value,
            tracks: tracks.value,
            playlists: playlists.value,
            videos: videos.value,
            top_hit_type: None,
            top_hits: top_hits.value,
        },
        complete,
    })
}

fn history_texts(data: &Value, included: &[Value]) -> Vec<SuggestionTextItem> {
    rel_idents(data, "history")
        .into_iter()
        .filter_map(|ident| {
            let resource = find_included(included, &ident.typ, &ident.id)?;
            let query = attr_str(resource, "query")
                .or_else(|| attr_str(resource, "name"))?
                .trim();
            if query.is_empty() {
                return None;
            }
            Some(SuggestionTextItem {
                query: query.to_string(),
                source: "history".into(),
            })
        })
        .collect()
}

fn suggestion_texts(resource: &Value, limit: u32) -> Vec<SuggestionTextItem> {
    let Some(items) = attr(resource, "suggestions").and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let query = item.get("query").and_then(Value::as_str)?.trim();
            if query.is_empty() {
                return None;
            }
            Some(SuggestionTextItem {
                query: query.to_string(),
                source: "suggestion".into(),
            })
        })
        .take(limit as usize)
        .collect()
}

fn map_suggestions_document(
    doc: &Value,
    limit: u32,
) -> Result<CatalogMap<SuggestionsResponse>, SoneError> {
    let data = resource_data(doc)?;
    if attr(data, "suggestions").is_none() {
        return Err(SoneError::Parse(
            "official suggestions missing suggestions".into(),
        ));
    }
    let mut text_suggestions = history_texts(data, included_slice(doc));
    text_suggestions.extend(suggestion_texts(data, limit));
    text_suggestions.truncate(limit as usize);
    let hits = take_limited(
        map_direct_hits(&rel_idents(data, "directHits"), included_slice(doc)),
        limit,
    );
    Ok(CatalogMap {
        value: SuggestionsResponse {
            text_suggestions,
            direct_hits: hits.value,
        },
        complete: hits.complete,
    })
}

fn map_user_document(doc: &Value) -> Result<(String, Option<String>), SoneError> {
    let data = resource_data(doc)?;
    let username = attr_str(data, "username")
        .filter(|username| !username.is_empty())
        .ok_or_else(|| SoneError::Parse("official user missing username".into()))?
        .to_string();
    let first = attr_str(data, "firstName").unwrap_or("");
    let last = attr_str(data, "lastName").unwrap_or("");
    let name = match (first.is_empty(), last.is_empty()) {
        (false, false) => format!("{first} {last}"),
        (false, true) => first.to_string(),
        _ => "TIDAL User".to_string(),
    };
    Ok((name, Some(username)))
}

fn playlist_refs(doc: &Value) -> Result<Vec<PlaylistItemRef>, SoneError> {
    idents_in_data(doc)?
        .into_iter()
        .map(|ident| {
            let item_id = ident
                .item_id
                .ok_or_else(|| SoneError::Parse("official playlist item missing itemId".into()))?;
            Ok(PlaylistItemRef {
                item_id,
                resource_id: ident.id,
                resource_type: canonical_media_type(&ident.typ)?.to_string(),
            })
        })
        .collect()
}

pub(super) fn playlist_add_body(ids: &[(String, String)], on_duplicates: &str) -> Value {
    json!({
        "data": ids.iter().map(|(id, typ)| json!({"id": id, "type": typ})).collect::<Vec<_>>(),
        "meta": {"onDuplicates": on_duplicates}
    })
}

pub(super) fn playlist_remove_body(item: &PlaylistItemRef) -> Value {
    json!({
        "data": [{
            "id": item.resource_id,
            "type": item.resource_type,
            "meta": {"itemId": item.item_id}
        }]
    })
}

fn member_body(kind: CollectionKind, id: &str) -> Value {
    json!({
        "data": [{ "id": id, "type": kind.item_type() }]
    })
}

fn sort_param(sort: PlaylistSort) -> Option<String> {
    match sort {
        PlaylistSort::Default | PlaylistSort::Unsupported => None,
        PlaylistSort::Field(field, true) => Some(format!("-{field}")),
        PlaylistSort::Field(field, false) => Some(field.to_string()),
    }
}

/// One official page is not aligned to the UI page. `pending` holds items
/// already downloaded past `origin`, and `next_cursor` fetches whatever
/// follows them.
#[derive(Debug)]
pub(super) struct PlaylistWalk {
    playlist_id: String,
    sort: Option<String>,
    origin: u32,
    pending: Vec<TidalTrack>,
    next_cursor: Option<String>,
    exhausted: bool,
}

impl PlaylistWalk {
    fn fresh(playlist_id: &str, sort: Option<&str>) -> Self {
        Self {
            playlist_id: playlist_id.to_string(),
            sort: sort.map(str::to_string),
            origin: 0,
            pending: Vec::new(),
            next_cursor: None,
            exhausted: false,
        }
    }

    fn covers(&self, offset: u32, limit: u32) -> bool {
        if self.exhausted {
            return true;
        }
        let Some(end) = self.covered_end() else {
            return false;
        };
        end >= offset.saturating_add(limit)
    }

    fn covered_end(&self) -> Option<u32> {
        let len = u32::try_from(self.pending.len()).ok()?;
        self.origin.checked_add(len)
    }

    fn page(&mut self, offset: u32, limit: u32) -> PaginatedTracks {
        let end = self.covered_end().unwrap_or(self.origin);
        let items = if offset >= end {
            Vec::new()
        } else {
            let start = (offset - self.origin) as usize;
            self.pending
                .iter()
                .skip(start)
                .take(limit as usize)
                .cloned()
                .collect()
        };
        let total = playlist_page_total(
            self.origin,
            self.pending.len(),
            offset,
            items.len() as u32,
            !self.exhausted,
        );
        if offset >= self.origin {
            let start = (offset - self.origin) as usize;
            let keep_from = start + items.len();
            if keep_from <= self.pending.len() {
                self.pending.drain(..keep_from);
                self.origin = offset + items.len() as u32;
            }
        }
        PaginatedTracks {
            items,
            total_number_of_items: total,
            offset,
            limit,
        }
    }
}

/// `more` means another official page still exists. Items sitting in `pending`
/// past this slice also mean the UI has another page, even when this response
/// already contained them.
fn is_abandoned_playlist_walk(err: &SoneError) -> bool {
    matches!(
        err,
        SoneError::Parse(message) if message == "official playlist page truncated"
    )
}

fn playlist_page_total(
    origin: u32,
    pending_len: usize,
    offset: u32,
    returned: u32,
    more: bool,
) -> u32 {
    let end = origin.saturating_add(u32::try_from(pending_len).unwrap_or(u32::MAX));
    let page_end = offset.saturating_add(returned);
    if more || page_end < end {
        page_end.saturating_add(1)
    } else {
        end
    }
}

fn track_has_cover(track: &TidalTrack) -> bool {
    track
        .album
        .as_ref()
        .and_then(|album| album.cover.as_deref())
        .is_some_and(|cover| !cover.is_empty())
}

impl super::TidalClient {
    fn begin_playlist_walk(&self, playlist_id: &str, sort: Option<&str>) -> PlaylistWalk {
        let mut guard = self
            .playlist_walk
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        match guard.take() {
            Some(walk) if walk.playlist_id == playlist_id && walk.sort.as_deref() == sort => walk,
            _ => PlaylistWalk::fresh(playlist_id, sort),
        }
    }

    fn save_playlist_walk(&self, walk: PlaylistWalk) {
        *self
            .playlist_walk
            .lock()
            .unwrap_or_else(|err| err.into_inner()) = Some(walk);
    }

    pub(super) fn invalidate_playlist_walk(&self, playlist_id: &str) {
        let mut guard = self
            .playlist_walk
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        if guard
            .as_ref()
            .is_some_and(|walk| walk.playlist_id == playlist_id)
        {
            *guard = None;
        }
    }

    async fn openapi_get(
        &mut self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<Value, SoneError> {
        let url = format!("{TIDAL_OPENAPI_URL}{path}");
        let response = self.authenticated_get(&url, query).await?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        json_status(path, status, body)
    }

    async fn openapi_get_shared(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<Value, SoneError> {
        let tokens = self.tokens.as_ref().ok_or(SoneError::NotAuthenticated)?;
        let (generation, client) = self.client_at()?;
        let url = format!("{TIDAL_OPENAPI_URL}{path}");
        let request = client
            .get(&url)
            .header("Authorization", format!("Bearer {}", tokens.access_token))
            .header("Accept", "application/vnd.api+json")
            .header("x-tidal-client-version", TIDAL_CLIENT_VERSION)
            .query(query);
        let response = self.send(generation, request).await?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        json_status(path, status, body)
    }

    async fn openapi_write(
        &self,
        method: reqwest::Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&Value>,
    ) -> Result<(), SoneError> {
        let tokens = self.tokens.as_ref().ok_or(SoneError::NotAuthenticated)?;
        let (generation, client) = self.client_at()?;
        let url = format!("{TIDAL_OPENAPI_URL}{path}");
        let mut request = client
            .request(method.clone(), &url)
            .header("Authorization", format!("Bearer {}", tokens.access_token))
            .header("Accept", "application/vnd.api+json")
            .header("x-tidal-client-version", TIDAL_CLIENT_VERSION)
            .query(query);
        if let Some(body) = body {
            let text =
                serde_json::to_string(body).map_err(|err| SoneError::Parse(err.to_string()))?;
            request = request
                .header("Content-Type", "application/vnd.api+json")
                .body(text);
        }
        let response = self.send(generation, request).await?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let text = response.text().await.unwrap_or_default();
        json_status(path, status, text).map(|_| ())
    }

    async fn playlist_items_doc(
        &mut self,
        playlist_id: &str,
        cursor: Option<&str>,
        sort: Option<&str>,
        include_items: bool,
    ) -> Result<Value, SoneError> {
        let country = self.country_code.clone();
        let path = format!("/playlists/{playlist_id}/relationships/items");
        let mut query = vec![("countryCode", country.as_str())];
        if include_items {
            query.push(("include", "items"));
        }
        if let Some(sort) = sort {
            query.push(("sort", sort));
        }
        if let Some(cursor) = cursor {
            query.push(("page[cursor]", cursor));
        }
        self.openapi_get(&path, &query).await
    }

    async fn playlist_items_doc_shared(
        &self,
        playlist_id: &str,
        cursor: Option<&str>,
        sort: Option<&str>,
        include_items: bool,
    ) -> Result<Value, SoneError> {
        let country = self.country_code.clone();
        let path = format!("/playlists/{playlist_id}/relationships/items");
        let mut query = vec![("countryCode", country.as_str())];
        if include_items {
            query.push(("include", "items"));
        }
        if let Some(sort) = sort {
            query.push(("sort", sort));
        }
        if let Some(cursor) = cursor {
            query.push(("page[cursor]", cursor));
        }
        self.openapi_get_shared(&path, &query).await
    }

    pub(super) async fn official_playlist_page(
        &mut self,
        playlist_id: &str,
        offset: u32,
        limit: u32,
        order: Option<&str>,
        order_direction: Option<&str>,
    ) -> Result<PaginatedTracks, SoneError> {
        let sort = match playlist_sort(order, order_direction) {
            PlaylistSort::Unsupported => {
                return Err(SoneError::Parse(
                    "unsupported official playlist sort".into(),
                ));
            }
            other => sort_param(other),
        };
        // Play-all and MCP ask for the whole list. One official page that
        // still has a cursor would turn into dozens of round trips, and the
        // private endpoint can finish that list with offset and limit.
        if offset == 0 && limit == u32::MAX {
            return self
                .official_playlist_full(playlist_id, sort.as_deref())
                .await;
        }
        let mut walk = self.begin_playlist_walk(playlist_id, sort.as_deref());
        if offset < walk.origin {
            walk = PlaylistWalk::fresh(playlist_id, sort.as_deref());
        }
        if walk.origin > 0 {
            log::debug!(
                "[openapi] playlist {playlist_id} continues from item {}",
                walk.origin
            );
        }
        if let Err(err) = self
            .fill_playlist_walk(&mut walk, playlist_id, sort.as_deref(), offset, limit)
            .await
        {
            if !is_abandoned_playlist_walk(&err) {
                self.save_playlist_walk(walk);
            }
            return Err(err);
        }
        let page = walk.page(offset, limit);
        self.save_playlist_walk(walk);
        Ok(page)
    }

    async fn official_playlist_full(
        &mut self,
        playlist_id: &str,
        sort: Option<&str>,
    ) -> Result<PaginatedTracks, SoneError> {
        let doc = self
            .playlist_items_doc(playlist_id, None, sort, true)
            .await?;
        let mapped = map_media_document(&doc)?;
        if !mapped.complete {
            return Err(SoneError::Parse(
                "official playlist items missing display fields".into(),
            ));
        }
        if next_cursor(&doc).is_some() {
            return Err(SoneError::Parse("official playlist page truncated".into()));
        }
        let total = mapped.value.len() as u32;
        Ok(PaginatedTracks {
            items: mapped.value,
            total_number_of_items: total,
            offset: 0,
            limit: total,
        })
    }

    async fn fill_playlist_walk(
        &mut self,
        walk: &mut PlaylistWalk,
        playlist_id: &str,
        sort: Option<&str>,
        offset: u32,
        limit: u32,
    ) -> Result<(), SoneError> {
        let mut pages = 0usize;
        while !walk.covers(offset, limit) {
            if pages == MAX_RELATION_PAGES {
                return Err(SoneError::Parse("official playlist page truncated".into()));
            }
            let doc = self
                .playlist_items_doc(playlist_id, walk.next_cursor.as_deref(), sort, true)
                .await?;
            let mapped = map_media_document(&doc)?;
            if !mapped.complete {
                return Err(SoneError::Parse(
                    "official playlist items missing display fields".into(),
                ));
            }
            let cursor = next_cursor(&doc);
            if cursor.is_some() && cursor == walk.next_cursor {
                return Err(SoneError::Parse("official playlist page truncated".into()));
            }
            if mapped.value.is_empty() {
                walk.exhausted = true;
                walk.next_cursor = None;
                break;
            }
            let added = u32::try_from(mapped.value.len()).unwrap_or(u32::MAX);
            if walk
                .covered_end()
                .and_then(|end| end.checked_add(added))
                .is_none()
            {
                return Err(SoneError::Parse("official playlist page truncated".into()));
            }
            walk.pending.extend(mapped.value);
            walk.next_cursor = cursor;
            walk.exhausted = walk.next_cursor.is_none();
            pages += 1;
        }
        Ok(())
    }

    pub(super) async fn official_playlist_tracks(
        &mut self,
        playlist_id: &str,
    ) -> Result<Vec<TidalTrack>, SoneError> {
        let page = self.official_playlist_full(playlist_id, None).await?;
        Ok(page.items)
    }

    pub(super) async fn official_playlist_details(
        &mut self,
        playlist_id: &str,
    ) -> Result<Value, SoneError> {
        let country = self.country_code.clone();
        let path = format!("/playlists/{playlist_id}");
        let doc = self
            .openapi_get(
                &path,
                &[
                    ("countryCode", country.as_str()),
                    ("include", "coverArt,owners"),
                ],
            )
            .await?;
        require_complete(map_playlist_details(&doc)?, "playlist")
    }

    pub(super) async fn official_add_playlist_tracks(
        &self,
        playlist_id: &str,
        track_ids: &[u64],
        on_duplicates: &str,
    ) -> Result<(), SoneError> {
        if track_ids.is_empty() {
            return Ok(());
        }
        let ids: Vec<(String, String)> = track_ids
            .iter()
            .map(|id| (id.to_string(), "tracks".to_string()))
            .collect();
        let body = playlist_add_body(&ids, on_duplicates);
        let country = self.country_code.clone();
        let path = format!("/playlists/{playlist_id}/relationships/items");
        self.openapi_write(
            reqwest::Method::POST,
            &path,
            &[("countryCode", country.as_str())],
            Some(&body),
        )
        .await
    }

    pub(super) async fn official_remove_playlist_item(
        &self,
        playlist_id: &str,
        item: &PlaylistItemRef,
    ) -> Result<(), SoneError> {
        let resource_type = canonical_media_type(&item.resource_type)?.to_string();
        let normalized = PlaylistItemRef {
            item_id: item.item_id.clone(),
            resource_id: item.resource_id.clone(),
            resource_type,
        };
        let body = playlist_remove_body(&normalized);
        let country = self.country_code.clone();
        let path = format!("/playlists/{playlist_id}/relationships/items");
        self.openapi_write(
            reqwest::Method::DELETE,
            &path,
            &[("countryCode", country.as_str())],
            Some(&body),
        )
        .await
    }

    async fn playlist_ref_at(
        &self,
        playlist_id: &str,
        index: u32,
    ) -> Result<PlaylistItemRef, SoneError> {
        let mut seen = 0u32;
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_RELATION_PAGES {
            let doc = self
                .playlist_items_doc_shared(playlist_id, cursor.as_deref(), Some("itemIndex"), false)
                .await?;
            let refs = playlist_refs(&doc)?;
            if refs.is_empty() {
                return Err(out_of_range());
            }
            if seen + refs.len() as u32 > index {
                return Ok(refs[(index - seen) as usize].clone());
            }
            seen += refs.len() as u32;
            cursor = next_cursor(&doc);
            if cursor.is_none() {
                return Err(out_of_range());
            }
        }
        Err(SoneError::Parse("official playlist index truncated".into()))
    }

    pub(super) async fn official_remove_playlist_index(
        &self,
        playlist_id: &str,
        index: u32,
    ) -> Result<(), SoneError> {
        let item = self.playlist_ref_at(playlist_id, index).await?;
        self.official_remove_playlist_item(playlist_id, &item).await
    }

    pub(super) async fn official_delete_playlist(
        &self,
        playlist_id: &str,
    ) -> Result<(), SoneError> {
        let country = self.country_code.clone();
        let path = format!("/playlists/{playlist_id}");
        self.openapi_write(
            reqwest::Method::DELETE,
            &path,
            &[("countryCode", country.as_str())],
            None,
        )
        .await
    }

    async fn walk_members(
        &self,
        kind: CollectionKind,
        user_id: u64,
        stop_at: Option<&str>,
    ) -> Result<MemberWalk, SoneError> {
        let path = format!("/{}/{user_id}/relationships/items", kind.resource());
        let mut ids = Vec::new();
        let mut cursor: Option<String> = None;
        for page in 0..MAX_RELATION_PAGES {
            // `data` on this relationship is already the identifier list.
            // `include=items` would embed every favorited resource.
            let doc = if let Some(cursor) = cursor.as_deref() {
                self.openapi_get_shared(&path, &[("page[cursor]", cursor)])
                    .await?
            } else {
                self.openapi_get_shared(&path, &[]).await?
            };
            let idents = idents_in_data(&doc)?;
            if idents.is_empty() {
                return Ok(MemberWalk {
                    ids,
                    truncated: false,
                });
            }
            for ident in idents {
                ids.push(ident.id);
                if stop_at.is_some_and(|needle| ids.last().is_some_and(|id| id == needle)) {
                    return Ok(MemberWalk {
                        ids,
                        truncated: false,
                    });
                }
            }
            cursor = next_cursor(&doc);
            if cursor.is_none() {
                return Ok(MemberWalk {
                    ids,
                    truncated: false,
                });
            }
            if page + 1 == MAX_RELATION_PAGES {
                break;
            }
        }
        Ok(MemberWalk {
            ids,
            truncated: cursor.is_some(),
        })
    }

    pub(super) async fn official_member_ids(
        &self,
        kind: CollectionKind,
        user_id: u64,
    ) -> Result<Vec<String>, SoneError> {
        let walked = self.walk_members(kind, user_id, None).await?;
        if walked.truncated {
            log::warn!(
                "[openapi] {} collection for {user_id} stopped after {} ids",
                kind.resource(),
                walked.ids.len()
            );
        }
        Ok(walked.ids)
    }

    pub(super) async fn official_member_contains(
        &self,
        kind: CollectionKind,
        user_id: u64,
        needle: &str,
    ) -> Result<bool, SoneError> {
        let walked = self.walk_members(kind, user_id, Some(needle)).await?;
        if walked.ids.iter().any(|id| id == needle) {
            return Ok(true);
        }
        if walked.truncated {
            return Err(SoneError::Parse("official collection truncated".into()));
        }
        Ok(false)
    }

    pub(super) async fn official_member_add(
        &self,
        kind: CollectionKind,
        user_id: u64,
        id: &str,
    ) -> Result<(), SoneError> {
        let path = format!("/{}/{user_id}/relationships/items", kind.resource());
        let body = member_body(kind, id);
        self.openapi_write(reqwest::Method::POST, &path, &[], Some(&body))
            .await
    }

    pub(super) async fn official_member_remove(
        &self,
        kind: CollectionKind,
        user_id: u64,
        id: &str,
    ) -> Result<(), SoneError> {
        let path = format!("/{}/{user_id}/relationships/items", kind.resource());
        let body = member_body(kind, id);
        self.openapi_write(reqwest::Method::DELETE, &path, &[], Some(&body))
            .await
    }

    pub(super) async fn official_album(
        &mut self,
        album_id: u64,
    ) -> Result<TidalAlbumDetail, SoneError> {
        let country = self.country_code.clone();
        let doc = self
            .openapi_get(
                &format!("/albums/{album_id}"),
                &[
                    ("countryCode", country.as_str()),
                    ("include", "artists,coverArt"),
                ],
            )
            .await?;
        require_complete(map_album_document(&doc)?, "album")
    }

    pub(super) async fn official_album_tracks(
        &mut self,
        album_id: u64,
        offset: u32,
        limit: u32,
    ) -> Result<PaginatedTracks, SoneError> {
        self.relationship_track_page(
            &format!("/albums/{album_id}/relationships/items"),
            "items",
            offset,
            limit,
        )
        .await
    }

    pub(super) async fn official_artist(
        &mut self,
        artist_id: u64,
    ) -> Result<TidalArtistDetail, SoneError> {
        let country = self.country_code.clone();
        let doc = self
            .openapi_get(
                &format!("/artists/{artist_id}"),
                &[("countryCode", country.as_str()), ("include", "profileArt")],
            )
            .await?;
        require_complete(map_artist_detail(&doc)?, "artist")
    }

    pub(super) async fn official_artist_albums(
        &mut self,
        artist_id: u64,
        limit: u32,
    ) -> Result<Vec<TidalAlbumDetail>, SoneError> {
        let page = self
            .relationship_album_page(
                &format!("/artists/{artist_id}/relationships/albums"),
                "albums",
                limit,
            )
            .await?;
        Ok(page)
    }

    pub async fn similar_albums(
        &mut self,
        album_id: u64,
    ) -> Result<Vec<TidalAlbumDetail>, SoneError> {
        self.relationship_album_page(
            &format!("/albums/{album_id}/relationships/similarAlbums"),
            "similarAlbums",
            20,
        )
        .await
    }

    pub async fn similar_tracks(&mut self, track_id: u64) -> Result<Vec<TidalTrack>, SoneError> {
        let page = self
            .relationship_track_page(
                &format!("/tracks/{track_id}/relationships/similarTracks"),
                "similarTracks",
                0,
                20,
            )
            .await?;
        Ok(page.items)
    }

    pub(super) async fn official_video(&mut self, video_id: u64) -> Result<TidalVideo, SoneError> {
        let country = self.country_code.clone();
        let doc = self
            .openapi_get(
                &format!("/videos/{video_id}"),
                &[
                    ("countryCode", country.as_str()),
                    ("include", "artists,thumbnailArt"),
                ],
            )
            .await?;
        require_complete(map_video_document(&doc)?, "video")
    }

    pub(super) async fn official_user(
        &mut self,
        user_id: u64,
    ) -> Result<(String, Option<String>), SoneError> {
        let doc = self.openapi_get(&format!("/users/{user_id}"), &[]).await?;
        map_user_document(&doc)
    }

    async fn relationship_track_page(
        &mut self,
        path: &str,
        include: &str,
        offset: u32,
        limit: u32,
    ) -> Result<PaginatedTracks, SoneError> {
        let country = self.country_code.clone();
        let want = offset.saturating_add(limit) as usize;
        let mut collected = Vec::new();
        let mut cursor: Option<String> = None;
        let mut more = true;
        for _ in 0..MAX_RELATION_PAGES {
            if collected.len() >= want || !more {
                break;
            }
            let doc = if let Some(cursor) = cursor.as_deref() {
                self.openapi_get(
                    path,
                    &[
                        ("countryCode", country.as_str()),
                        ("include", include),
                        ("page[cursor]", cursor),
                    ],
                )
                .await?
            } else {
                self.openapi_get(
                    path,
                    &[("countryCode", country.as_str()), ("include", include)],
                )
                .await?
            };
            let mapped = map_media_document(&doc)?;
            if !mapped.complete {
                return Err(SoneError::Parse(format!(
                    "official {include} missing display fields"
                )));
            }
            if mapped.value.is_empty() {
                more = false;
                break;
            }
            collected.extend(mapped.value);
            cursor = next_cursor(&doc);
            more = cursor.is_some();
        }
        if more && collected.len() < want {
            return Err(SoneError::Parse(format!("official {include} truncated")));
        }
        let known = collected.len() as u32;
        let items: Vec<TidalTrack> = collected
            .into_iter()
            .skip(offset as usize)
            .take(limit as usize)
            .collect();
        let total = if more {
            offset + items.len() as u32 + 1
        } else {
            known
        };
        Ok(PaginatedTracks {
            items,
            total_number_of_items: total,
            offset,
            limit,
        })
    }

    async fn relationship_album_page(
        &mut self,
        path: &str,
        include: &str,
        limit: u32,
    ) -> Result<Vec<TidalAlbumDetail>, SoneError> {
        let country = self.country_code.clone();
        let want = limit as usize;
        let mut collected = Vec::new();
        let mut cursor: Option<String> = None;
        let mut more = true;
        for _ in 0..MAX_RELATION_PAGES {
            if collected.len() >= want || !more {
                break;
            }
            let doc = if let Some(cursor) = cursor.as_deref() {
                self.openapi_get(
                    path,
                    &[
                        ("countryCode", country.as_str()),
                        ("include", include),
                        ("page[cursor]", cursor),
                    ],
                )
                .await?
            } else {
                self.openapi_get(
                    path,
                    &[("countryCode", country.as_str()), ("include", include)],
                )
                .await?
            };
            let mapped = if doc.get("data").and_then(Value::as_object).is_some()
                && doc
                    .get("data")
                    .and_then(|data| data.get("attributes"))
                    .is_some()
            {
                return Err(SoneError::Parse(format!(
                    "official {include} returned a resource instead of a relationship"
                )));
            } else {
                let idents = idents_in_data(&doc)?;
                map_album_idents(&idents, included_slice(&doc))
            };
            if !mapped.complete {
                return Err(SoneError::Parse(format!(
                    "official {include} missing display fields"
                )));
            }
            if mapped.value.is_empty() {
                more = false;
                break;
            }
            collected.extend(mapped.value);
            cursor = next_cursor(&doc);
            more = cursor.is_some();
        }
        if more && collected.len() < want {
            return Err(SoneError::Parse(format!("official {include} truncated")));
        }
        collected.truncate(want);
        Ok(collected)
    }

    pub(super) async fn official_search(
        &mut self,
        query: &str,
        limit: u32,
    ) -> Result<TidalSearchResults, SoneError> {
        let country = self.country_code.clone();
        let doc = self
            .openapi_get(
                "/searchResults",
                &[
                    ("filter[query]", query),
                    ("countryCode", country.as_str()),
                    ("include", "albums,artists,playlists,topHits,tracks,videos"),
                    ("explicitFilter", "INCLUDE"),
                    ("deviceType", "DESKTOP"),
                ],
            )
            .await?;
        if let Some(suggestion) = resource_data(&doc)
            .ok()
            .and_then(|data| attr_str(data, "didYouMean"))
            .filter(|text| !text.is_empty())
        {
            log::debug!("[openapi] search didYouMean: {suggestion}");
        }
        let data = resource_data(&doc)?.clone();
        let search_id = data
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let mapped = map_search_document(&doc, limit)?;
        if !mapped.complete {
            return Err(SoneError::Parse(
                "official search missing display fields".into(),
            ));
        }
        let mut results = mapped.value;
        results.artists = self
            .extend_search(
                &search_id,
                "artists",
                cursor_unless_full(&data, "artists", results.artists.len(), limit),
                results.artists,
                limit,
                map_artist_page,
            )
            .await?;
        results.albums = self
            .extend_search(
                &search_id,
                "albums",
                cursor_unless_full(&data, "albums", results.albums.len(), limit),
                results.albums,
                limit,
                map_album_page,
            )
            .await?;
        results.tracks = self
            .extend_search(
                &search_id,
                "tracks",
                cursor_unless_full(&data, "tracks", results.tracks.len(), limit),
                results.tracks,
                limit,
                map_track_page,
            )
            .await?;
        results.playlists = self
            .extend_search(
                &search_id,
                "playlists",
                cursor_unless_full(&data, "playlists", results.playlists.len(), limit),
                results.playlists,
                limit,
                map_playlist_page,
            )
            .await?;
        results.videos = self
            .extend_search(
                &search_id,
                "videos",
                cursor_unless_full(&data, "videos", results.videos.len(), limit),
                results.videos,
                limit,
                map_video_page,
            )
            .await?;
        results.top_hits = self
            .extend_search(
                &search_id,
                "topHits",
                cursor_unless_full(&data, "topHits", results.top_hits.len(), limit),
                results.top_hits,
                limit,
                map_hit_page,
            )
            .await?;
        Ok(results)
    }

    async fn extend_search<T, F>(
        &mut self,
        search_id: &str,
        relationship: &str,
        mut cursor: Option<String>,
        mut items: Vec<T>,
        limit: u32,
        mut map_page: F,
    ) -> Result<Vec<T>, SoneError>
    where
        F: FnMut(&Value) -> Result<CatalogMap<Vec<T>>, SoneError>,
    {
        if search_id.is_empty() {
            items.truncate(limit as usize);
            return Ok(items);
        }
        let country = self.country_code.clone();
        for _ in 0..MAX_SEARCH_PAGES {
            if items.len() >= limit as usize {
                break;
            }
            let Some(current) = cursor.clone() else {
                break;
            };
            let path = format!("/searchResults/{search_id}/relationships/{relationship}");
            let doc = self
                .openapi_get(
                    &path,
                    &[
                        ("page[cursor]", current.as_str()),
                        ("countryCode", country.as_str()),
                        ("include", relationship),
                    ],
                )
                .await?;
            let mapped = map_page(&doc)?;
            if !mapped.complete {
                return Err(SoneError::Parse(format!(
                    "official search {relationship} missing display fields"
                )));
            }
            if mapped.value.is_empty() {
                break;
            }
            items.extend(mapped.value);
            cursor = next_cursor(&doc);
        }
        items.truncate(limit as usize);
        Ok(items)
    }

    pub(super) async fn official_suggestions(
        &mut self,
        query: &str,
        limit: u32,
    ) -> Result<SuggestionsResponse, SoneError> {
        let country = self.country_code.clone();
        let doc = self
            .openapi_get(
                "/searchSuggestions",
                &[
                    ("filter[query]", query),
                    ("countryCode", country.as_str()),
                    ("include", "directHits,history"),
                    ("explicitFilter", "INCLUDE"),
                ],
            )
            .await?;
        require_complete(map_suggestions_document(&doc, limit)?, "suggestions")
    }

    async fn load_user_mix(
        &mut self,
        collection: &str,
        id: &str,
        title: &str,
        mix_type: &str,
        cover_only: bool,
    ) -> Result<Option<MixPageResult>, SoneError> {
        let path = format!("/{collection}/{id}");
        let doc = self
            .openapi_get(&path, &[("include", "items"), ("locale", "en-US")])
            .await?;
        let data = resource_data(&doc)?;
        let mix_id = data
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| SoneError::Parse("official user mix missing id".into()))?
            .to_string();
        let first = map_ident_slice(&rel_idents(data, "items"), included_slice(&doc));
        if !first.complete {
            return Err(SoneError::Parse(
                "official user mix missing display fields".into(),
            ));
        }
        let mut tracks = first.value;
        let mut cursor = rel_cursor(data, "items");
        // Favorite-mix cards only keep the first album cover. Opening the mix
        // loads the tracks through `official_mix_by_id`.
        if !(cover_only && tracks.iter().any(track_has_cover)) {
            for _ in 0..MAX_MIX_PAGES {
                let Some(current) = cursor.clone() else {
                    break;
                };
                let rel = format!("/{collection}/{mix_id}/relationships/items");
                let page = self
                    .openapi_get(
                        &rel,
                        &[
                            ("page[cursor]", current.as_str()),
                            ("include", "items"),
                            ("locale", "en-US"),
                        ],
                    )
                    .await?;
                let mapped = map_media_document(&page)?;
                if !mapped.complete {
                    return Err(SoneError::Parse(
                        "official user mix missing display fields".into(),
                    ));
                }
                if mapped.value.is_empty() {
                    break;
                }
                tracks.extend(mapped.value);
                cursor = next_cursor(&page);
                if cover_only && tracks.iter().any(track_has_cover) {
                    break;
                }
            }
        }
        if tracks.is_empty() {
            return Ok(None);
        }
        if cover_only {
            if let Some(index) = tracks.iter().position(track_has_cover) {
                tracks.truncate(index + 1);
            }
        }
        let image = tracks.iter().find_map(|track| {
            track
                .album
                .as_ref()
                .and_then(|album| album.cover.clone())
                .map(|cover| resource_image(&cover, 320))
        });
        Ok(Some(MixPageResult {
            mix_id,
            mix_type: Some(mix_type.to_string()),
            title: Some(title.to_string()),
            subtitle: Some(String::new()),
            image,
            tracks,
        }))
    }

    pub async fn list_user_mixes(&mut self) -> Result<Vec<MixPageResult>, SoneError> {
        let mut mixes = Vec::new();
        for (collection, title, mix_type) in USER_MIXES {
            match self
                .load_user_mix(collection, "me", title, mix_type, true)
                .await
            {
                Ok(Some(page)) => mixes.push(page),
                Ok(None) => {}
                Err(err) if !fallback_after(&err) => return Err(err),
                Err(err) => {
                    log::warn!("[list_user_mixes] {collection} failed ({err})");
                }
            }
        }
        Ok(mixes)
    }

    pub(super) async fn official_mix_by_id(
        &mut self,
        mix_id: &str,
    ) -> Result<Option<MixPageResult>, SoneError> {
        for (collection, title, mix_type) in USER_MIXES {
            match self
                .load_user_mix(collection, mix_id, title, mix_type, false)
                .await
            {
                Ok(Some(page)) => return Ok(Some(page)),
                Ok(None) => continue,
                Err(SoneError::Api { status: 404, .. }) => continue,
                Err(err) if !fallback_after(&err) => return Err(err),
                Err(err) => {
                    log::warn!("[official_mix_by_id] {collection}/{mix_id} failed ({err})");
                }
            }
        }
        Ok(None)
    }
}

fn cursor_unless_full(
    resource: &Value,
    relationship: &str,
    len: usize,
    limit: u32,
) -> Option<String> {
    if len >= limit as usize {
        None
    } else {
        rel_cursor(resource, relationship)
    }
}

fn map_artist_page(doc: &Value) -> Result<CatalogMap<Vec<TidalArtist>>, SoneError> {
    let idents = idents_in_data(doc)?;
    Ok(map_artist_idents(&idents, included_slice(doc)))
}

fn map_album_page(doc: &Value) -> Result<CatalogMap<Vec<TidalAlbumDetail>>, SoneError> {
    let idents = idents_in_data(doc)?;
    Ok(map_album_idents(&idents, included_slice(doc)))
}

fn map_track_page(doc: &Value) -> Result<CatalogMap<Vec<TidalTrack>>, SoneError> {
    map_media_document(doc)
}

fn map_playlist_page(doc: &Value) -> Result<CatalogMap<Vec<TidalPlaylist>>, SoneError> {
    let idents = idents_in_data(doc)?;
    Ok(map_playlist_idents(&idents, included_slice(doc)))
}

fn map_video_page(doc: &Value) -> Result<CatalogMap<Vec<TidalVideo>>, SoneError> {
    let idents = idents_in_data(doc)?;
    Ok(map_video_idents(&idents, included_slice(doc)))
}

fn map_hit_page(doc: &Value) -> Result<CatalogMap<Vec<DirectHitItem>>, SoneError> {
    let idents = idents_in_data(doc)?;
    Ok(map_direct_hits(&idents, included_slice(doc)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artist_json() -> Value {
        json!({
            "type": "artists",
            "id": "7",
            "attributes": {"name": "Artist", "handle": "artist", "popularity": 0.5},
            "relationships": {
                "profileArt": {"data": [{"id": "pic-uuid", "type": "artworks"}]}
            }
        })
    }

    fn album_json() -> Value {
        json!({
            "type": "albums",
            "id": "99",
            "attributes": {
                "title": "Album",
                "albumType": "ALBUM",
                "duration": "PT40M",
                "explicit": false,
                "numberOfItems": 10,
                "numberOfVolumes": 1,
                "barcodeId": "123",
                "releaseDate": "2020-01-02",
                "popularity": 0.25,
                "copyright": {"text": "2020 Label"}
            },
            "relationships": {
                "artists": {"data": [{"id": "7", "type": "artists"}]},
                "coverArt": {"data": [{"id": "cover-uuid", "type": "artworks"}]}
            }
        })
    }

    fn track_json() -> Value {
        json!({
            "type": "tracks",
            "id": "123",
            "attributes": {
                "title": "Song",
                "duration": "PT3M45S",
                "explicit": true,
                "popularity": 0.8,
                "isrc": "USXXX",
                "version": "Radio",
                "copyright": {"text": "2020 Label"},
                "mediaTags": ["LOSSLESS", "DOLBY_ATMOS"]
            },
            "relationships": {
                "artists": {"data": [{"id": "7", "type": "artists"}]},
                "albums": {"data": [{"id": "99", "type": "albums"}]}
            }
        })
    }

    #[test]
    fn duration_and_popularity_match_openapi_units() {
        assert_eq!(iso8601_duration_secs("PT3M45S"), Some(225));
        assert_eq!(iso8601_duration_secs("pt1h2m3s"), Some(3723));
        assert_eq!(iso8601_duration_secs("PT1.5S"), Some(2));
        assert_eq!(iso8601_duration_secs("P1DT2H"), Some(93_600));
        assert_eq!(iso8601_duration_secs("PT0S"), Some(0));
        assert_eq!(iso8601_duration_secs("225"), Some(225));
        assert_eq!(iso8601_duration_secs("PT"), None);
        assert_eq!(iso8601_duration_secs("nope"), None);
        assert_eq!(popularity_percent(0.8), Some(80));
        assert_eq!(popularity_percent(1.0), Some(100));
        assert_eq!(popularity_percent(42.0), Some(42));
        assert_eq!(popularity_percent(-1.0), None);
        assert_eq!(popularity_percent(f64::NAN), None);
    }

    #[test]
    fn next_cursor_reads_meta_and_link() {
        let meta = json!({"links": {"self": "/x", "meta": {"nextCursor": "abc"}}});
        assert_eq!(next_cursor(&meta).as_deref(), Some("abc"));
        let link =
            json!({"links": {"next": "https://openapi.tidal.com/v2/x?page%5Bcursor%5D=a%2Fb"}});
        assert_eq!(next_cursor(&link).as_deref(), Some("a/b"));
        assert_eq!(next_cursor(&json!({"links": {"self": "/x"}})), None);
    }

    #[test]
    fn playlist_sort_uses_official_fields() {
        assert_eq!(playlist_sort(None, None), PlaylistSort::Default);
        assert_eq!(
            playlist_sort(Some("ARTIST"), Some("DESC")),
            PlaylistSort::Field("artists.name", true)
        );
        assert_eq!(
            playlist_sort(Some("DATE"), Some("ASC")),
            PlaylistSort::Field("addedAt", false)
        );
        assert_eq!(
            playlist_sort(Some("INDEX"), None),
            PlaylistSort::Field("itemIndex", false)
        );
        assert_eq!(playlist_sort(Some("MOOD"), None), PlaylistSort::Unsupported);
    }

    #[test]
    fn playlist_page_total_counts_leftovers_and_open_cursors() {
        assert_eq!(playlist_page_total(0, 100, 0, 100, true), 101);
        assert_eq!(playlist_page_total(0, 40, 0, 40, false), 40);
        assert_eq!(playlist_page_total(100, 50, 100, 50, false), 150);
        assert_eq!(playlist_page_total(0, 250, 0, 100, false), 101);
        assert_eq!(playlist_page_total(150, 0, 200, 0, false), 150);
    }

    #[test]
    fn playlist_walk_keeps_the_tail_for_the_next_page() {
        let doc = json!({
            "data": [{
                "id": "123",
                "type": "tracks",
                "meta": {"itemId": "item-1", "addedAt": "2024-01-02T00:00:00Z"}
            }],
            "included": [track_json(), album_json(), artist_json()]
        });
        let track = map_media_document(&doc).unwrap().value.remove(0);
        let mut walk = PlaylistWalk::fresh("playlist", None);
        walk.pending = vec![track.clone(), track.clone(), track];
        walk.exhausted = true;
        let page = walk.page(0, 2);
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.total_number_of_items, 3);
        assert_eq!(walk.origin, 2);
        assert_eq!(walk.pending.len(), 1);
        let next = walk.page(2, 2);
        assert_eq!(next.items.len(), 1);
        assert_eq!(next.total_number_of_items, 3);
        assert!(walk.pending.is_empty());
        assert_eq!(walk.origin, 3);
    }

    #[test]
    fn fallback_stops_for_rate_limit_and_auth() {
        assert!(!fallback_after(&SoneError::Api {
            status: 429,
            body: String::new(),
        }));
        assert!(!fallback_after(&SoneError::NotAuthenticated));
        assert!(fallback_after(&SoneError::Api {
            status: 403,
            body: String::new(),
        }));
        assert!(fallback_after(&SoneError::Parse("missing".into())));
    }

    #[test]
    fn playlist_payloads_use_item_identity() {
        let body = playlist_add_body(&[("123".into(), "tracks".into())], "FAIL");
        assert_eq!(body["data"][0]["id"], "123");
        assert_eq!(body["data"][0]["type"], "tracks");
        assert_eq!(body["meta"]["onDuplicates"], "FAIL");
        let remove = playlist_remove_body(&PlaylistItemRef {
            item_id: "item-1".into(),
            resource_id: "123".into(),
            resource_type: "tracks".into(),
        });
        assert_eq!(remove["data"][0]["meta"]["itemId"], "item-1");
        assert_eq!(remove["data"][0]["id"], "123");
    }

    #[test]
    fn playlist_items_keep_item_id_duration_and_cover() {
        let doc = json!({
            "data": [{
                "id": "123",
                "type": "tracks",
                "meta": {"itemId": "item-1", "addedAt": "2024-01-02T00:00:00Z"}
            }],
            "included": [track_json(), album_json(), artist_json()],
            "links": {"self": "/items", "meta": {"nextCursor": "next"}}
        });
        let mapped = map_media_document(&doc).unwrap();
        assert!(mapped.complete);
        let track = &mapped.value[0];
        assert_eq!(track.id, 123);
        assert_eq!(track.title, "Song");
        assert_eq!(track.duration, 225);
        assert_eq!(track.playlist_item_id.as_deref(), Some("item-1"));
        assert_eq!(track.popularity, Some(80));
        assert_eq!(track.audio_quality.as_deref(), Some("LOSSLESS"));
        assert_eq!(track.explicit, Some(true));
        assert_eq!(
            track.artist.as_ref().map(|artist| artist.name.as_str()),
            Some("Artist")
        );
        assert_eq!(
            track
                .album
                .as_ref()
                .and_then(|album| album.cover.as_deref()),
            Some("cover-uuid")
        );
        assert_eq!(next_cursor(&doc).as_deref(), Some("next"));
    }

    #[test]
    fn playlist_items_without_artists_are_incomplete() {
        let doc = json!({
            "data": [{"id": "123", "type": "tracks", "meta": {"itemId": "item-1"}}],
            "included": [track_json()]
        });
        let mapped = map_media_document(&doc).unwrap();
        assert!(!mapped.complete);
    }

    #[test]
    fn album_track_video_and_user_documents_map_into_client_types() {
        let album = map_album_document(&json!({
            "data": album_json(),
            "included": [artist_json()]
        }))
        .unwrap();
        assert!(album.complete);
        assert_eq!(album.value.cover.as_deref(), Some("cover-uuid"));
        assert_eq!(album.value.duration, Some(2400));
        assert_eq!(album.value.popularity, Some(25));
        assert_eq!(album.value.upc.as_deref(), Some("123"));
        assert_eq!(
            album
                .value
                .artist
                .as_ref()
                .map(|artist| artist.name.as_str()),
            Some("Artist")
        );

        let artist = map_artist_detail(&json!({"data": artist_json()})).unwrap();
        assert!(artist.complete);
        assert_eq!(artist.value.picture.as_deref(), Some("pic-uuid"));
        assert_eq!(artist.value.popularity, Some(50));

        let video_resource = json!({
            "type": "videos",
            "id": "55",
            "attributes": {"title": "Video", "duration": "PT1M2S", "explicit": false},
            "relationships": {
                "artists": {"data": [{"id": "7", "type": "artists"}]},
                "thumbnailArt": {"data": [{"id": "thumb-uuid", "type": "artworks"}]}
            }
        });
        let video = map_video_document(&json!({
            "data": video_resource,
            "included": [artist_json()]
        }))
        .unwrap();
        assert!(video.complete);
        assert_eq!(video.value.duration, Some(62));
        assert_eq!(video.value.image_id.as_deref(), Some("thumb-uuid"));

        let (name, username) = map_user_document(&json!({
            "data": {
                "type": "users",
                "id": "9",
                "attributes": {"username": "ada", "firstName": "Ada", "lastName": "Lovelace", "country": "US"}
            }
        }))
        .unwrap();
        assert_eq!(name, "Ada Lovelace");
        assert_eq!(username.as_deref(), Some("ada"));
    }

    #[test]
    fn playlist_details_keep_owner_cover_and_counts() {
        let doc = json!({
            "data": {
                "type": "playlists",
                "id": "playlist-uuid",
                "attributes": {
                    "name": "Night",
                    "description": "Late",
                    "accessType": "PUBLIC",
                    "playlistType": "USER",
                    "duration": "PT3M45S",
                    "numberOfItems": 2,
                    "numberOfTrackItems": 1,
                    "numberOfVideoItems": 1,
                    "lastModifiedAt": "2024-05-01T00:00:00Z"
                },
                "relationships": {
                    "coverArt": {"data": [{"id": "cover-uuid", "type": "artworks"}]},
                    "owners": {"data": [{"id": "42", "type": "users"}]}
                }
            },
            "included": [{
                "type": "users",
                "id": "42",
                "attributes": {"username": "ada", "name": "Ada"}
            }]
        });
        let mapped = map_playlist_details(&doc).unwrap();
        assert!(mapped.complete);
        assert_eq!(mapped.value["title"], "Night");
        assert_eq!(mapped.value["squareImage"], "cover-uuid");
        assert_eq!(mapped.value["numberOfTracks"], 1);
        assert_eq!(mapped.value["numberOfVideos"], 1);
        assert_eq!(mapped.value["duration"], 225);
        assert_eq!(mapped.value["creator"]["id"], 42);
        assert_eq!(mapped.value["accessType"], "PUBLIC");
        assert_eq!(mapped.value["type"], "USER");
    }

    #[test]
    fn search_and_suggestions_resolve_included_hits() {
        let search = json!({
            "data": {
                "type": "searchResults",
                "id": "search-1",
                "attributes": {"query": "artist", "trackingId": "t", "didYouMean": "Artist"},
                "relationships": {
                    "artists": {"data": [{"id": "7", "type": "artists"}]},
                    "albums": {"data": [{"id": "99", "type": "albums"}]},
                    "tracks": {"data": [{"id": "123", "type": "tracks"}]},
                    "playlists": {"data": [{"id": "playlist-uuid", "type": "playlists"}]},
                    "videos": {"data": []},
                    "topHits": {"data": [{"id": "123", "type": "tracks"}]}
                }
            },
            "included": [
                artist_json(),
                album_json(),
                track_json(),
                {
                    "type": "playlists",
                    "id": "playlist-uuid",
                    "attributes": {"name": "Night", "numberOfTrackItems": 1},
                    "relationships": {
                        "coverArt": {"data": [{"id": "cover-uuid", "type": "artworks"}]}
                    }
                }
            ]
        });
        let mapped = map_search_document(&search, 50).unwrap();
        assert!(mapped.complete);
        assert_eq!(mapped.value.artists[0].name, "Artist");
        assert_eq!(mapped.value.albums[0].title, "Album");
        assert_eq!(mapped.value.tracks[0].id, 123);
        assert_eq!(mapped.value.playlists[0].uuid, "playlist-uuid");
        assert_eq!(mapped.value.top_hits[0].hit_type, "TRACKS");
        assert_eq!(
            mapped.value.top_hits[0].album_cover.as_deref(),
            Some("cover-uuid")
        );

        let suggestions = json!({
            "data": {
                "type": "searchSuggestions",
                "id": "sugg-1",
                "attributes": {
                    "query": "ar",
                    "trackingId": "t",
                    "suggestions": [
                        {"query": "artist", "highlights": [{"start": 0, "length": 2}]}
                    ]
                },
                "relationships": {
                    "directHits": {"data": [{"id": "7", "type": "artists"}]}
                }
            },
            "included": [artist_json()]
        });
        let mapped = map_suggestions_document(&suggestions, 10).unwrap();
        assert!(mapped.complete);
        assert_eq!(mapped.value.text_suggestions[0].query, "artist");
        assert_eq!(mapped.value.direct_hits[0].hit_type, "ARTISTS");
        assert_eq!(mapped.value.direct_hits[0].name.as_deref(), Some("Artist"));
    }
}
