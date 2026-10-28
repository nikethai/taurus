//! The URLs one job downloads from: its link and its mirrors, and which of
//! them have failed during this run.

use crate::errors::CoreError;

/// `[primary] + mirrors`. A URL marked bad stays bad for the rest of the run.
pub(crate) struct Sources {
    urls: Vec<String>,
    bad: Vec<bool>,
    primary_error: Option<CoreError>,
    /// The last error of any mirror.
    mirror_error: Option<CoreError>,
}

impl Sources {
    pub fn new(primary: &str, mirrors: &[String]) -> Sources {
        let urls: Vec<String> = std::iter::once(primary.to_owned())
            .chain(mirrors.iter().cloned())
            .collect();
        Sources {
            bad: vec![false; urls.len()],
            urls,
            primary_error: None,
            mirror_error: None,
        }
    }

    pub fn url(&self, source: usize) -> &str {
        &self.urls[source]
    }

    /// The source for segment `idx`: `idx % len`, or the next good one after
    /// it. `None` once every URL is bad.
    pub fn pick(&self, idx: usize) -> Option<usize> {
        let len = self.urls.len();
        (0..len)
            .map(|step| (idx + step) % len)
            .find(|&source| !self.bad[source])
    }

    /// Records the error and marks the source bad; false once no good
    /// source remains.
    pub fn mark_bad(&mut self, source: usize, e: CoreError) -> bool {
        tracing::info!(
            url = self.urls[source].as_str(),
            "not using this URL any more: {e}"
        );
        self.bad[source] = true;
        if source == 0 {
            self.primary_error = Some(e);
        } else {
            self.mirror_error = Some(e);
        }
        self.bad.iter().any(|bad| !bad)
    }

    pub fn len(&self) -> usize {
        self.urls.len()
    }

    /// The job's error once every source is bad: the primary's last error,
    /// else the last error seen.
    pub fn error(&self) -> CoreError {
        self.primary_error
            .clone()
            .or_else(|| self.mirror_error.clone())
            .unwrap_or_else(|| CoreError {
                kind: crate::errors::ErrorKind::Unknown,
                detail: "no URL to download from".to_owned(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::ErrorKind;

    fn err(detail: &str) -> CoreError {
        CoreError {
            kind: ErrorKind::LinkExpired,
            detail: detail.to_owned(),
        }
    }

    fn sources(mirrors: usize) -> Sources {
        let mirrors: Vec<String> = (1..=mirrors).map(|i| format!("http://m{i}/f")).collect();
        Sources::new("http://p/f", &mirrors)
    }

    #[test]
    fn segments_spread_round_robin_and_skip_bad_sources() {
        let mut s = sources(2);
        assert_eq!(
            (0..5).map(|i| s.pick(i)).collect::<Vec<_>>(),
            [Some(0), Some(1), Some(2), Some(0), Some(1)]
        );
        assert_eq!(s.url(2), "http://m2/f");
        assert!(s.mark_bad(1, err("m1")));
        // Segment 1 moves on to the next good source, not back to the primary.
        assert_eq!(s.pick(1), Some(2));
        assert_eq!(s.pick(4), Some(2));
        assert!(s.mark_bad(2, err("m2")));
        assert_eq!(s.pick(1), Some(0));
        assert!(!s.mark_bad(0, err("p")));
        assert_eq!(s.pick(0), None);
    }

    #[test]
    fn error_is_the_primarys_when_it_failed() {
        let mut s = sources(1);
        s.mark_bad(0, err("primary 410"));
        s.mark_bad(1, err("mirror 404"));
        assert_eq!(s.error().detail, "primary 410");

        let mut s = sources(2);
        s.mark_bad(2, err("m2"));
        s.mark_bad(1, err("m1"));
        assert_eq!(s.error().detail, "m1");
    }

    #[test]
    fn error_can_be_asked_for_again() {
        // A round may already have stopped for another reason when the last
        // URL goes bad; the next round asks for the error again.
        let mut s = sources(1);
        s.mark_bad(0, err("primary 410"));
        assert_eq!(s.error().detail, "primary 410");
        s.mark_bad(1, err("mirror 404"));
        assert_eq!(s.error().detail, "primary 410");
        assert_eq!(s.error().detail, "primary 410");
    }

    #[test]
    fn without_mirrors_the_first_failure_ends_it() {
        let mut s = sources(0);
        assert_eq!(s.pick(7), Some(0));
        assert!(!s.mark_bad(0, err("gone")));
        assert_eq!(s.error().detail, "gone");
    }
}
