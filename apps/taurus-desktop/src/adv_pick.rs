//! What is picked on the Advanced Add screen (pure): the options of a
//! file link, a video or a torrent (its files in a `torrent_pick::Picker`),
//! the screen's view model and the engine request they make.

use std::path::PathBuf;

use slint::{ModelRc, SharedString, VecModel};
use taurus_core::checksum::parse_sum;
use taurus_core::media::{
    Container, LiveStatus, MediaInfo, QualityOption, SubtitleChoice, standard_quality,
    validate_browser,
};
use taurus_core::plan::DEFAULT_CONNECTIONS;
use taurus_core::torrent::TorrentInfo;
use taurus_core::{
    AddRequest, Credentials, ErrorKind, JobKind, LinkInfo, MediaChoice, TorrentOverrides,
};

use crate::detail::with_override;
use crate::torrent_pick::Picker;
use crate::{AdvAddVm, LangVm, QualityVm, format, rows};

/// The connection counts the screen offers.
const CONNECTIONS: [u32; 4] = [1, 4, 8, 16];
/// The "Keep sharing" choices: the setting's, ratio 0 (stop when done),
/// 1, 2, or no limit.
const RATIOS: [&str; 5] = ["default", "0", "1", "2", "never"];
/// The browsers the screen offers to sign in with.
const BROWSERS: [&str; 5] = ["chrome", "firefox", "safari", "edge", "brave"];

/// What the link turned out to be.
pub(crate) enum AdvSource {
    File {
        suggested: String,
        size: Option<u64>,
        /// The link asked for a user name and password when looked at.
        needs_user: bool,
    },
    Media(Box<MediaInfo>),
    Torrent(TorrentInfo),
}

/// Everything picked on the Advanced Add screen. The password is never
/// here: it goes from the window straight into the request.
pub(crate) struct AdvPick {
    source: AdvSource,
    // File links.
    name: String,
    connections: u32,
    mirrors: Vec<String>,
    checksum: String,
    referer: String,
    user: String,
    // Videos.
    qualities: Vec<QualityOption>,
    quality: String,
    container: Container,
    subs: Vec<String>,
    auto_subs: bool,
    embed_subs: bool,
    cookies: String,
    // Torrents.
    picker: Picker,
    ratio: String,
    /// The add went to the engine and has not answered yet.
    pub(crate) busy: bool,
    /// The engine refused the add: why.
    pub(crate) refused: Option<ErrorKind>,
}

/// The qualities a video offers: the link's best, one option per height,
/// then its audio options. A link without heights (a playlist, a song)
/// offers what it lists.
fn quality_list(info: &MediaInfo) -> Vec<QualityOption> {
    if info.all_qualities.is_empty() {
        if info.qualities.is_empty() {
            return ["best", "audio-best"]
                .into_iter()
                .filter_map(standard_quality)
                .collect();
        }
        return info.qualities.clone();
    }
    let best = info.qualities.iter().filter(|q| q.id == "best");
    let audio = info.qualities.iter().filter(|q| q.audio_only);
    best.chain(&info.all_qualities)
        .chain(audio)
        .cloned()
        .collect()
}

/// 720 for "720p"; 0 for any other id.
fn height_of(id: &str) -> i32 {
    id.strip_suffix('p')
        .filter(|h| !h.is_empty() && h.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|h| h.parse().ok())
        .unwrap_or(0)
}

fn container_str(c: Container) -> &'static str {
    c.format()
}

fn parse_container(s: &str) -> Option<Container> {
    match s {
        "mp4" => Some(Container::Mp4),
        "mkv" => Some(Container::Mkv),
        "webm" => Some(Container::Webm),
        _ => None,
    }
}

/// The last part of a link's path, as a file name ("download" when it
/// has none).
fn name_from_link(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| {
            u.path_segments()
                .and_then(|mut s| s.next_back().map(str::to_owned))
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "download".to_owned())
}

fn model<T: Clone + 'static>(rows: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(rows))
}

impl AdvPick {
    fn new(source: AdvSource) -> AdvPick {
        let mut pick = AdvPick {
            source,
            name: String::new(),
            connections: DEFAULT_CONNECTIONS,
            mirrors: Vec::new(),
            checksum: String::new(),
            referer: String::new(),
            user: String::new(),
            qualities: Vec::new(),
            quality: String::new(),
            container: Container::Mp4,
            subs: Vec::new(),
            auto_subs: false,
            embed_subs: false,
            cookies: String::new(),
            picker: Picker::default(),
            ratio: "default".to_owned(),
            busy: false,
            refused: None,
        };
        match &pick.source {
            AdvSource::File { suggested, .. } => pick.name = suggested.clone(),
            AdvSource::Media(info) => {
                pick.qualities = quality_list(info);
                // A source without video starts on its best audio.
                let all_audio = pick.qualities.iter().all(|q| q.audio_only);
                pick.quality = pick
                    .qualities
                    .iter()
                    .find(|q| q.audio_only == all_audio)
                    .or(pick.qualities.first())
                    .map(|q| q.id.clone())
                    .unwrap_or_else(|| "best".to_owned());
            }
            AdvSource::Torrent(info) => pick.picker = Picker::from_info(info),
        }
        pick
    }

    /// What `Engine::inspect` found.
    pub(crate) fn of(info: &LinkInfo) -> AdvPick {
        AdvPick::new(match info {
            LinkInfo::File { name, size, .. } => AdvSource::File {
                suggested: name.clone(),
                size: *size,
                needs_user: false,
            },
            LinkInfo::Media(m) => AdvSource::Media(Box::new(m.clone())),
            LinkInfo::Torrent(t) => AdvSource::Torrent(t.clone()),
        })
    }

    /// A web link that asked for a user name and password.
    pub(crate) fn signed_in_file(url: &str) -> AdvPick {
        AdvPick::new(AdvSource::File {
            suggested: name_from_link(url),
            size: None,
            needs_user: true,
        })
    }

    /// The browser the link was read with: videos sign in with it too.
    pub(crate) fn with_cookies(mut self, browser: &str) -> AdvPick {
        if matches!(self.source, AdvSource::Media(_)) {
            self.cookies = browser.to_owned();
        }
        self
    }

    pub(crate) fn picker_mut(&mut self) -> &mut Picker {
        &mut self.picker
    }

    fn info(&self) -> Option<&MediaInfo> {
        match &self.source {
            AdvSource::Media(info) => Some(info),
            _ => None,
        }
    }

    fn audio_only(&self) -> bool {
        self.qualities
            .iter()
            .find(|q| q.id == self.quality)
            .is_some_and(|q| q.audio_only)
    }

    fn checksum_ok(&self) -> bool {
        let c = self.checksum.trim();
        c.is_empty() || parse_sum(c).is_some()
    }

    /// The add can go to the engine.
    pub(crate) fn ready(&self) -> bool {
        !self.busy
            && match self.source {
                AdvSource::File { .. } => self.checksum_ok(),
                AdvSource::Media(_) => true,
                AdvSource::Torrent(_) => self.picker.selected_count() > 0,
            }
    }

    /// "file" | "media" | "torrent".
    pub(crate) fn kind(&self) -> &'static str {
        match self.source {
            AdvSource::File { .. } => "file",
            AdvSource::Media(_) => "media",
            AdvSource::Torrent(_) => "torrent",
        }
    }

    /// The kind the destination is sorted by.
    pub(crate) fn folder_kind(&self) -> &'static str {
        match &self.source {
            AdvSource::File { .. } => rows::kind_for(self.name.trim(), JobKind::Http),
            AdvSource::Media(_) if self.audio_only() => "music",
            AdvSource::Media(_) => "media",
            AdvSource::Torrent(_) => "magnet",
        }
    }

    fn badge(&self) -> &'static str {
        match self.info() {
            Some(info)
                if matches!(
                    info.live,
                    LiveStatus::IsLive | LiveStatus::IsUpcoming { .. }
                ) =>
            {
                "live"
            }
            _ => self.folder_kind(),
        }
    }

    /// The subtitle languages shown: the uploader's, then (with the
    /// generated captions on) those only the site's captions have.
    fn langs(&self) -> Vec<(String, bool)> {
        let Some(info) = self.info() else {
            return Vec::new();
        };
        let own = info.subtitle_languages.iter().map(|l| (l.clone(), false));
        let auto = info
            .auto_caption_languages
            .iter()
            .filter(|l| self.auto_subs && !info.subtitle_languages.contains(l))
            .map(|l| (l.clone(), true));
        own.chain(auto).collect()
    }

    /// Changes one choice (`AdvAddVm` field names, plus "sub-lang", which
    /// toggles a language, and "user"); false when the value is not one
    /// the screen offers.
    pub(crate) fn set(&mut self, field: &str, value: &str) -> bool {
        let on = value == "true";
        match field {
            "name" => self.name = value.to_owned(),
            "connections" => match value.parse() {
                Ok(n) if CONNECTIONS.contains(&n) => self.connections = n,
                _ => return false,
            },
            "mirrors" => {
                self.mirrors = value
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(str::to_owned)
                    .collect();
            }
            "checksum" => self.checksum = value.to_owned(),
            "referer" => self.referer = value.to_owned(),
            "user" => self.user = value.to_owned(),
            "quality" if self.qualities.iter().any(|q| q.id == value) => {
                self.quality = value.to_owned();
            }
            "container" => match parse_container(value) {
                Some(c) => self.container = c,
                None => return false,
            },
            "sub-lang" if self.langs().iter().any(|(l, _)| l == value) => {
                match self.subs.iter().position(|l| l == value) {
                    Some(i) => {
                        self.subs.remove(i);
                    }
                    None => self.subs.push(value.to_owned()),
                }
            }
            "auto-subs" => {
                self.auto_subs = on;
                let shown = self.langs();
                self.subs.retain(|l| shown.iter().any(|(s, _)| s == l));
            }
            "embed-subs" => self.embed_subs = on,
            "cookies-browser"
                if value.is_empty()
                    || (BROWSERS.contains(&value) && validate_browser(value).is_ok()) =>
            {
                self.cookies = value.to_owned();
            }
            "ratio" if RATIOS.contains(&value) => self.ratio = value.to_owned(),
            _ => return false,
        }
        true
    }

    /// The screen, except the folder, its free space and the picture
    /// (the app knows those).
    pub(crate) fn vm(&self) -> AdvAddVm {
        let mut vm = AdvAddVm {
            kind: self.kind().into(),
            badge: self.badge().into(),
            busy: self.busy,
            refused: self.refused.map_or("", ErrorKind::as_str).into(),
            checksum_ok: true,
            ratio: self.ratio.as_str().into(),
            ..Default::default()
        };
        match &self.source {
            AdvSource::File {
                size, needs_user, ..
            } => {
                vm.name = self.name.as_str().into();
                vm.size_text = size.map(format::bytes).unwrap_or_default().into();
                vm.connections = self.connections as i32;
                vm.mirrors = model(
                    self.mirrors
                        .iter()
                        .map(|m| SharedString::from(m.as_str()))
                        .collect(),
                );
                vm.checksum = self.checksum.as_str().into();
                vm.checksum_ok = self.checksum_ok();
                vm.referer = self.referer.as_str().into();
                vm.needs_user = *needs_user;
            }
            AdvSource::Media(info) => {
                let audio = self.audio_only();
                vm.title = info.title.as_str().into();
                vm.name = info.title.as_str().into();
                vm.uploader = info.uploader.as_deref().unwrap_or("").into();
                vm.duration_text = info
                    .duration_secs
                    .map(format::duration)
                    .unwrap_or_default()
                    .into();
                vm.live = match info.live {
                    LiveStatus::IsLive => "live",
                    LiveStatus::IsUpcoming { .. } => "upcoming",
                    _ => "",
                }
                .into();
                vm.playlist_count = info
                    .playlist
                    .as_ref()
                    .map_or(0, |p| p.count.min(i32::MAX as u32) as i32);
                vm.size_text = self
                    .qualities
                    .iter()
                    .find(|q| q.id == self.quality)
                    .and_then(|q| q.est_size)
                    .map(format::bytes)
                    .unwrap_or_default()
                    .into();
                vm.qualities = model(
                    self.qualities
                        .iter()
                        .map(|q| QualityVm {
                            id: q.id.as_str().into(),
                            height: height_of(&q.id),
                            audio_only: q.audio_only,
                            size_text: q.est_size.map(format::bytes).unwrap_or_default().into(),
                        })
                        .collect(),
                );
                vm.quality_id = self.quality.as_str().into();
                vm.audio_only = audio;
                vm.container = container_str(self.container).into();
                vm.subtitle_langs = model(
                    self.langs()
                        .into_iter()
                        .map(|(code, auto)| LangVm {
                            selected: self.subs.contains(&code),
                            code: code.into(),
                            auto,
                        })
                        .collect(),
                );
                vm.auto_subs = self.auto_subs;
                vm.embed_subs = self.embed_subs;
                vm.cookies_browser = self.cookies.as_str().into();
            }
            AdvSource::Torrent(info) => {
                vm.name = info.name.as_str().into();
                vm.size_text = format::bytes(info.total).into();
                vm.files = model(self.picker.rows());
                vm.selected_count = self.picker.selected_count().min(i32::MAX as usize) as i32;
                vm.selected_size = format::bytes(self.picker.selected_size()).into();
            }
        }
        vm
    }

    /// What the selected torrent files still need on disk (0 for other
    /// links: the engine checks those).
    pub(crate) fn needed(&self) -> u64 {
        match self.source {
            AdvSource::Torrent(_) => self.picker.needed(),
            _ => 0,
        }
    }

    /// The engine's request. `password` is used only with a user name.
    pub(crate) fn request(&self, url: &str, dest_dir: PathBuf, password: String) -> AddRequest {
        let mut req = AddRequest {
            url: url.to_owned(),
            dest_dir,
            ..Default::default()
        };
        let text = |s: &str| Some(s.trim().to_owned()).filter(|s| !s.is_empty());
        match &self.source {
            AdvSource::File { suggested, .. } => {
                req.file_name = text(&self.name).filter(|n| n != suggested);
                req.connections = Some(self.connections);
                req.mirrors = self.mirrors.clone();
                req.checksum = text(&self.checksum);
                req.referer = text(&self.referer);
                req.credentials =
                    text(&self.user).map(|username| Credentials { username, password });
            }
            AdvSource::Media(info) => {
                let audio = self.audio_only();
                let subtitles = (!self.subs.is_empty()).then(|| SubtitleChoice {
                    languages: self.subs.clone(),
                    auto_generated: self.auto_subs,
                    embed: self.embed_subs,
                });
                req.media = Some(MediaChoice {
                    quality: self.quality.clone(),
                    convert: None,
                    cookies_from_browser: text(&self.cookies),
                    subtitles,
                    container: (!audio).then_some(self.container),
                });
                req.media_info = Some((**info).clone());
                // The screen shows how many videos a playlist has.
                req.confirm_large_playlist = true;
            }
            AdvSource::Torrent(_) => {
                req.only_files = (!self.picker.all_selected()).then(|| self.picker.chosen());
            }
        }
        req
    }

    /// The torrent's own sharing rule picked ("Keep sharing"), if any.
    pub(crate) fn overrides(&self) -> Option<TorrentOverrides> {
        match self.source {
            AdvSource::Torrent(_) if self.ratio != "default" => {
                with_override(&TorrentOverrides::default(), "ratio", &self.ratio)
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slint::Model;
    use taurus_core::torrent::TorrentFile;

    fn torrent() -> TorrentInfo {
        let file = |index, path: &str, size| TorrentFile {
            index,
            path: path.into(),
            size,
        };
        TorrentInfo {
            info_hash: "0".repeat(40),
            name: "pack".into(),
            // Index order is not path order.
            files: vec![
                file(0, "b.bin", 300),
                file(1, "a/2.bin", 200),
                file(2, "a/1.bin", 100),
            ],
            total: 600,
        }
    }

    fn quality(id: &str, audio_only: bool) -> QualityOption {
        QualityOption {
            id: id.into(),
            selector: String::new(),
            audio_only,
            est_size: Some(1_000_000),
        }
    }

    fn video() -> MediaInfo {
        MediaInfo {
            url: "https://www.youtube.com/watch?v=x".into(),
            title: "A Fine Video".into(),
            uploader: Some("Some Channel".into()),
            duration_secs: Some(212),
            thumbnail_url: None,
            extractor: "Youtube".into(),
            live: LiveStatus::NotLive,
            playlist: None,
            qualities: vec![
                quality("best", false),
                quality("1080p", false),
                quality("720p", false),
                quality("audio-best", true),
                quality("audio-small", true),
            ],
            subtitle_languages: vec!["en".into(), "vi".into()],
            auto_caption_languages: vec!["en".into(), "fr".into()],
            all_qualities: vec![
                quality("2160p", false),
                quality("1080p", false),
                quality("720p", false),
                quality("480p", false),
            ],
        }
    }

    #[test]
    fn video_choices_make_the_request() {
        let mut pick = AdvPick::of(&LinkInfo::Media(video()));
        let vm = pick.vm();
        let ids: Vec<String> = vm.qualities.iter().map(|q| q.id.to_string()).collect();
        assert_eq!(
            ids,
            [
                "best",
                "2160p",
                "1080p",
                "720p",
                "480p",
                "audio-best",
                "audio-small"
            ]
        );
        let heights: Vec<i32> = vm.qualities.iter().map(|q| q.height).collect();
        assert_eq!(heights, [0, 2160, 1080, 720, 480, 0, 0]);
        assert_eq!(
            (vm.quality_id.as_str(), vm.badge.as_str()),
            ("best", "media")
        );
        assert!(!pick.set("quality", "999p"));
        assert!(
            !pick.set("sub-lang", "fr"),
            "fr is a generated caption only"
        );
        for (f, v) in [
            ("quality", "720p"),
            ("container", "mkv"),
            ("auto-subs", "true"),
            ("sub-lang", "fr"),
            ("sub-lang", "en"),
            ("embed-subs", "true"),
            ("cookies-browser", "firefox"),
        ] {
            assert!(pick.set(f, v), "{f}={v}");
        }
        let langs: Vec<(String, bool, bool)> = pick
            .vm()
            .subtitle_langs
            .iter()
            .map(|l| (l.code.to_string(), l.auto, l.selected))
            .collect();
        assert_eq!(
            langs,
            [
                ("en".into(), false, true),
                ("vi".into(), false, false),
                ("fr".into(), true, true)
            ]
        );
        assert!(!pick.set("cookies-browser", "netscape"));
        let req = pick.request(&video().url, PathBuf::from("/d"), String::new());
        let choice = req.media.unwrap();
        assert_eq!(
            choice,
            MediaChoice {
                quality: "720p".into(),
                convert: None,
                cookies_from_browser: Some("firefox".into()),
                subtitles: Some(SubtitleChoice {
                    languages: vec!["fr".into(), "en".into()],
                    auto_generated: true,
                    embed: true,
                }),
                container: Some(Container::Mkv),
            }
        );
        assert!(req.media_info.is_some() && req.confirm_large_playlist);
        // Generated captions off: their languages go.
        pick.set("auto-subs", "false");
        let req = pick.request("u", PathBuf::from("/d"), String::new());
        let subs = req.media.unwrap().subtitles.unwrap();
        assert_eq!(subs.languages, ["en"]);
        // Audio: no container; the destination is Music.
        pick.set("quality", "audio-best");
        assert_eq!(pick.folder_kind(), "music");
        let req = pick.request("u", PathBuf::from("/d"), String::new());
        assert_eq!(req.media.unwrap().container, None);
    }

    #[test]
    fn file_choices_make_the_request() {
        let info = LinkInfo::File {
            url: "https://h/x.iso".into(),
            name: "x.iso".into(),
            size: Some(2_000_000),
            content_type: None,
        };
        let mut pick = AdvPick::of(&info);
        let vm = pick.vm();
        assert_eq!(
            (
                vm.kind.as_str(),
                vm.badge.as_str(),
                vm.connections,
                vm.checksum_ok
            ),
            ("file", "disc", 16, true)
        );
        // Untouched: the server's name, no sign-in.
        let req = pick.request("https://h/x.iso", PathBuf::from("/d"), "pw".into());
        assert_eq!((req.file_name, req.credentials), (None, None));
        assert!(!pick.set("connections", "3"));
        pick.set("checksum", "sha256:12");
        assert!(!pick.vm().checksum_ok && !pick.ready());
        let sum = "ab".repeat(32);
        for (f, v) in [
            ("connections", "8"),
            ("mirrors", " https://m1/x.iso \n\nhttps://m2/x.iso\n"),
            ("checksum", sum.as_str()),
            ("referer", " https://h/page "),
            ("name", "y.iso"),
            ("user", "me"),
        ] {
            assert!(pick.set(f, v), "{f}");
        }
        assert!(pick.ready());
        let req = pick.request("https://h/x.iso", PathBuf::from("/d"), "pw".into());
        assert_eq!(req.file_name.as_deref(), Some("y.iso"));
        assert_eq!(req.connections, Some(8));
        assert_eq!(req.mirrors, ["https://m1/x.iso", "https://m2/x.iso"]);
        assert_eq!(req.checksum.as_deref(), Some(sum.as_str()));
        assert_eq!(req.referer.as_deref(), Some("https://h/page"));
        let creds = req.credentials.unwrap();
        assert_eq!(
            (creds.username.as_str(), creds.password.as_str()),
            ("me", "pw")
        );

        let signed = AdvPick::signed_in_file("https://h/files/secret.zip?k=1");
        let vm = signed.vm();
        assert!(vm.needs_user);
        assert_eq!(
            (vm.name.as_str(), vm.badge.as_str()),
            ("secret.zip", "archive")
        );
    }

    #[test]
    fn torrent_choices_make_the_request() {
        let mut pick = AdvPick::of(&LinkInfo::Torrent(torrent()));
        let req = pick.request("magnet:?x", PathBuf::from("/d"), String::new());
        assert_eq!(req.only_files, None, "all files: no selection");
        assert_eq!(pick.overrides(), None);
        pick.picker_mut().toggle_group("a");
        assert!(pick.set("ratio", "2"));
        assert!(!pick.set("ratio", "7"));
        let vm = pick.vm();
        assert_eq!((vm.selected_count, vm.selected_size.as_str()), (1, "300 B"));
        let req = pick.request("magnet:?x", PathBuf::from("/d"), String::new());
        assert_eq!(req.only_files, Some(vec![0]));
        assert_eq!(pick.overrides().unwrap().stop_ratio, Some(Some(2.0)));
        pick.picker_mut().toggle_all(false);
        assert!(!pick.ready());
    }
}
