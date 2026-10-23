//! Zoom interaction and bounded page refinement. No queue of historical zoom requests.
use std::sync::Arc;
use wordcraft_doc::Document;
use wordcraft_layout::DocLayout;
use wordcraft_render::{RenderOptions, Rendered};

pub(crate) const SETTLE_SECONDS: f64 = 0.12;
/// One result plus conversion/upload is at most about 16 MiB. Renderer scratch, shared
/// document/media and prepared drawing commands are separate from this pixel-buffer cap.
pub(crate) const MAX_JOB_PIXELS: usize = 2 * 1024 * 1024;

#[derive(Default)]
pub(crate) struct Zoom {
    until: f64,
    raster: Option<f32>,
    pub(crate) continuous_command: bool,
}

impl Zoom {
    pub fn gesture(&mut self, now: f64) {
        self.until = if now.is_finite() { now + SETTLE_SECONDS } else { 0.0 };
    }
    pub fn active(&self, now: f64) -> bool {
        now < self.until
    }
    pub fn finish(&mut self) {
        self.until = 0.0;
    }
    pub fn raster_scale(&mut self, target: f32, now: f64) -> f32 {
        if !self.active(now) || self.raster.is_none() {
            self.raster = Some(target);
        }
        self.raster.unwrap_or(target)
    }
    pub fn remaining(&self, now: f64) -> std::time::Duration {
        let remaining = self.until - now;
        if remaining.is_finite() { std::time::Duration::from_secs_f64(remaining.clamp(0.0, SETTLE_SECONDS)) } else { std::time::Duration::ZERO }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Key {
    pub document: u64,
    pub revision: u64,
    pub content: u64,
    pub page: usize,
    pub scale: u32,
    pub tile: usize,
    pub height: usize,
}

struct Job {
    key: Key,
    doc: Arc<Document>,
    layout: Arc<DocLayout>,
    opts: RenderOptions,
    area: [f32; 4],
    ctx: egui::Context,
}

struct Completion {
    key: Key,
    image: Option<Rendered>,
    prepare_ms: f64,
    raster_ms: f64,
}

#[derive(Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Metrics {
    pub submitted: u64,
    pub completed: u64,
    pub discarded: u64,
    pub preview_frames: u64,
    pub prepare_ms: f64,
    pub raster_ms: f64,
    pub cache_bytes: usize,
    pub foreground_jobs: u64,
    pub pixels_uploaded: u64,
    pub conversion_ms: f64,
    pub texture_submit_ms: f64,
}

#[derive(Default)]
pub(crate) struct Refinement {
    #[cfg(not(target_arch = "wasm32"))]
    worker: Option<(std::sync::mpsc::SyncSender<Job>, std::sync::mpsc::Receiver<Completion>)>,
    busy: Option<Key>,
    ready: Option<Completion>,
    snapshot: Option<(u64, u64, Arc<Document>)>,
    failed: bool,
    document: Option<u64>,
    #[cfg(target_arch = "wasm32")]
    prepared: Prepared,
    pub metrics: Metrics,
}

impl Refinement {
    pub fn begin_frame(&mut self, document: u64, revision: u64) {
        if self.document != Some(document) {
            #[cfg(target_arch = "wasm32")]
            {
                self.prepared = None;
            }
            // Let a running job finish and reject its document id on delivery. Replacing
            // documents repeatedly must not start overlapping detached workers.
            self.ready = None;
            self.document = Some(document);
        }
        if self.snapshot.as_ref().is_some_and(|(id, rev, _)| (*id, *rev) != (document, revision)) {
            self.snapshot = None;
        }
        if self.ready.take().is_some() {
            self.metrics.discarded += 1;
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some((_, rx)) = &self.worker {
            match rx.try_recv() {
                Ok(done) => {
                    self.busy = None;
                    self.metrics.completed += 1;
                    self.metrics.prepare_ms = done.prepare_ms;
                    self.metrics.raster_ms = done.raster_ms;
                    self.ready = Some(done);
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.failed = true;
                    self.busy = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
    }

    /// At most one admitted job and one completed pixel buffer. Only the visible page visited
    /// by the canvas can admit the next job; obsolete requests are never queued behind it.
    pub fn request(
        &mut self,
        key: Key,
        doc: &Document,
        layout: &Arc<DocLayout>,
        opts: RenderOptions,
        area: [f32; 4],
        ctx: &egui::Context,
    ) -> Option<Rendered> {
        if self.ready.as_ref().is_some_and(|r| r.key == key) {
            return self.ready.take().and_then(|r| r.image);
        }
        if self.failed || self.busy.is_some() {
            return None;
        }
        let scale = f32::from_bits(key.scale);
        let w = (area[2] * scale).ceil() as usize;
        let h = (area[3] * scale).ceil() as usize;
        if w.checked_mul(h).is_none_or(|n| n == 0 || n > MAX_JOB_PIXELS) {
            return None;
        }
        if self.snapshot.as_ref().is_none_or(|(id, rev, _)| (*id, *rev) != (key.document, key.revision)) {
            self.snapshot = Some((key.document, key.revision, Arc::new(doc.clone())));
        }
        let Some((_, _, snapshot)) = &self.snapshot else { return None };
        let job = Job { key, doc: snapshot.clone(), layout: layout.clone(), opts, area, ctx: ctx.clone() };
        #[cfg(not(target_arch = "wasm32"))]
        {
            if self.worker.is_none() {
                let (tx, requests) = std::sync::mpsc::sync_channel::<Job>(1);
                let (results, rx) = std::sync::mpsc::sync_channel(1);
                match std::thread::Builder::new().name("wordcraft-refine".into()).spawn(move || {
                    let mut prepared = None;
                    while let Ok(job) = requests.recv() {
                        let ctx = job.ctx.clone();
                        // Last-resort containment; ordinary errors are represented by None.
                        let done = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| render(job, &mut prepared)));
                        let Ok(done) = done else {
                            ctx.request_repaint();
                            break;
                        };
                        if results.send(done).is_err() {
                            break;
                        }
                        ctx.request_repaint();
                    }
                }) {
                    Ok(_) => self.worker = Some((tx, rx)),
                    Err(e) => {
                        log::warn!("Could not start page refinement: {e}");
                        self.failed = true;
                        return None;
                    }
                }
            }
            if let Some((tx, _)) = &self.worker {
                if tx.try_send(job).is_ok() {
                    self.busy = Some(key);
                    self.metrics.submitted += 1;
                } else {
                    self.failed = true;
                }
            }
            None
        }
        #[cfg(target_arch = "wasm32")]
        {
            let done = render(job, &mut self.prepared);
            self.metrics.submitted += 1;
            self.metrics.completed += 1;
            self.metrics.prepare_ms = done.prepare_ms;
            self.metrics.raster_ms = done.raster_ms;
            done.image
        }
    }

    pub fn failed(&self) -> bool {
        self.failed
    }
}

type Prepared = Option<((u64, u64, u64, usize), wordcraft_render::PreparedPage)>;

fn render(job: Job, prepared: &mut Prepared) -> Completion {
    let _ = &job.ctx;
    let start = crate::now_ms();
    let Some(page) = job.layout.pages.get(job.key.page) else {
        return Completion { key: job.key, image: None, prepare_ms: 0.0, raster_ms: 0.0 };
    };
    let identity = (job.key.document, job.key.revision, job.key.content, job.key.page);
    if prepared.as_ref().is_none_or(|(k, _)| *k != identity) {
        *prepared = Some((identity, wordcraft_render::PreparedPage::new(&job.doc, page, &job.opts)));
    }
    let prepare_ms = crate::now_ms() - start;
    let start = crate::now_ms();
    let image = prepared.as_ref().map(|(_, p)| p.render_area(&job.doc, page, job.area, f32::from_bits(job.key.scale), &job.opts));
    Completion { key: job.key, image, prepare_ms, raster_ms: crate::now_ms() - start }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn continuous_zoom_keeps_raster_until_settled() {
        let mut z = Zoom::default();
        assert_eq!(z.raster_scale(2.0, 1.0), 2.0);
        z.gesture(1.0);
        assert_eq!(z.raster_scale(3.0, 1.01), 2.0);
        z.gesture(1.1);
        assert_eq!(z.raster_scale(4.0, 1.21), 2.0);
        assert_eq!(z.raster_scale(4.0, 1.23), 4.0);
        z.gesture(2.0);
        z.finish();
        assert_eq!(z.raster_scale(5.0, 2.0), 5.0);
        for time in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            z.gesture(time);
            assert_eq!(z.remaining(time), std::time::Duration::ZERO);
        }
    }

    fn key() -> Key {
        Key { document: 1, revision: 2, content: 3, page: 0, scale: 1.0f32.to_bits(), tile: 0, height: 0 }
    }

    #[test]
    fn rejects_oversized_jobs_before_snapshot_or_worker_creation() {
        let mut r = Refinement::default();
        let doc = Document::new();
        let layout = Arc::new(DocLayout::default());
        assert!(r.request(key(), &doc, &layout, RenderOptions::default(), [0.0, 0.0, 8192.0, 8192.0], &egui::Context::default()).is_none());
        assert!(r.snapshot.is_none());
        assert!(r.busy.is_none());
        assert_eq!(r.metrics.submitted, 0);
    }

    #[test]
    fn stale_completion_cannot_replace_new_document_or_revision() {
        let mut r = Refinement {
            document: Some(1),
            ready: Some(Completion { key: key(), image: None, prepare_ms: 0.0, raster_ms: 0.0 }),
            snapshot: Some((1, 2, Arc::new(Document::new()))),
            ..Default::default()
        };
        r.begin_frame(9, 0);
        assert!(r.ready.is_none());
        assert!(r.snapshot.is_none());
        assert!(r.busy.is_none());
        r.snapshot = Some((9, 0, Arc::new(Document::new())));
        r.begin_frame(9, 1);
        assert!(r.snapshot.is_none(), "edits release the old document snapshot");
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn worker_refines_without_blocking_foreground_and_only_delivers_matching_pixels() {
        let doc = Document::from_text("Foreground and refinement");
        let layout = Arc::new(wordcraft_layout::layout(&doc, &mut wordcraft_layout::LayoutCache::new(), &Default::default()));
        let ctx = egui::Context::default();
        let mut r = Refinement::default();
        r.begin_frame(1, 2);
        let area = [0.0, 0.0, 200.0, 150.0];
        assert!(r.request(key(), &doc, &layout, RenderOptions::default(), area, &ctx).is_none());
        let expected = wordcraft_render::render_area(&doc, &layout.pages[0], 0.0, 0.0, 200.0, 150.0, 1.0, &RenderOptions::default());
        let done = r.worker.as_ref().unwrap().1.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        r.busy = Some(Key { revision: 3, ..key() });
        r.ready = Some(done);
        assert!(
            r.request(Key { revision: 3, ..key() }, &doc, &layout, RenderOptions::default(), area, &ctx).is_none(),
            "old revision is never presented"
        );
        let image = r.request(key(), &doc, &layout, RenderOptions::default(), area, &ctx).unwrap();
        assert_eq!(image.pixels, expected.pixels);
        assert_eq!(r.metrics.submitted, 1, "no requests queue behind the running job");
    }
}
