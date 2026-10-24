//! A clip's sound, decoded into memory while it's already being played.
//!
//! Decoding a whole track takes the time to read the whole file (sound and
//! picture are interleaved in it): ~0.6 s for 5 minutes from the disk cache,
//! seconds from a hard drive. Waiting for it held up opening the player. Here
//! every track fills in from the start, and what's decoded is readable at once:
//! playback starts with the first chunk.
//!
//! No locks, so the audio callback never waits on the decoder: samples are
//! stored as `AtomicU32` (an f32's bits; a relaxed load is a plain load), and
//! each track publishes how far it's filled after each chunk.

use std::io::Read;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use anyhow::{Context, Result};

use crate::PREVIEW_RATE;

/// Sound being decoded right now, by any clip.
static DECODING: AtomicUsize = AtomicUsize::new(0);

/// Files whose sound is being decoded, and the flag that stops it.
static RUNNING: std::sync::Mutex<Vec<(std::path::PathBuf, Arc<AtomicBool>)>> = std::sync::Mutex::new(Vec::new());

/// Stop decoding `source`'s sound (it's about to be deleted: Windows won't
/// delete a file ffmpeg has open). What's decoded stays with whoever plays
/// it; the cache forgets it, so a decode cut short is never a clip's sound.
pub fn release(source: &Path) {
    if let Ok(mut kept) = DECODED.lock() {
        kept.retain(|s| s.path != source);
    }
    for (path, cancel) in RUNNING.lock().unwrap().iter() {
        if path == source {
            cancel.store(true, Ordering::Relaxed);
        }
    }
}

/// Some clip's sound is being decoded. Work that reads whole clips in the
/// background (scrub previews) waits for it: sharing a hard drive, they made
/// each other several times slower (12 s for a 2-minute clip's sound); one
/// after the other, the second reads the file from memory.
pub fn busy() -> bool {
    DECODING.load(Ordering::Relaxed) > 0
}

/// One track: stereo at [`PREVIEW_RATE`], each frame as two 16-bit samples
/// in one atomic (left in the low half). Half the memory of 32-bit floats
/// (a 10-minute clip's four tracks took 920 MB in the editor), and plenty
/// for listening: [`SCALE`] keeps one bit of headroom, so sound over full
/// scale (AAC can decode past it) isn't clipped, with its steps at -84 dBFS,
/// under the AAC sound's own noise.
pub struct PcmTrack {
    frames: Box<[AtomicU32]>,
    /// How many samples are decoded (2 per stereo frame).
    len: AtomicUsize,
}

/// Full scale (1.0) as a stored sample: values up to ±2.0 fit.
const SCALE: f32 = 16_383.0;

#[inline]
fn pack(l: f32, r: f32) -> u32 {
    let q = |v: f32| (v * SCALE).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16 as u16 as u32;
    q(l) | q(r) << 16
}

#[inline]
fn unpack(v: u32) -> (f32, f32) {
    ((v as u16 as i16) as f32 / SCALE, ((v >> 16) as u16 as i16) as f32 / SCALE)
}

impl PcmTrack {
    /// Room for `frames` stereo frames. The memory is zeroed by the system as
    /// it's first written, not up front: a clip being opened uses only as much
    /// as has been decoded.
    fn with_frames(frames: usize) -> Self {
        let zeroed = vec![0u32; frames].into_boxed_slice();
        // SAFETY: AtomicU32 has the same size, alignment and bit validity as u32.
        let frames = unsafe { Box::from_raw(Box::into_raw(zeroed) as *mut [AtomicU32]) };
        Self { frames, len: AtomicUsize::new(0) }
    }

    /// Samples decoded so far (2 per stereo frame).
    pub fn len(&self) -> usize {
        self.len.load(Ordering::Acquire)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Stereo frame `i`, if it's decoded.
    #[inline]
    pub fn frame(&self, i: usize) -> Option<(f32, f32)> {
        if 2 * i + 1 >= self.len() {
            return None;
        }
        Some(unpack(self.frames[i].load(Ordering::Relaxed)))
    }

    /// Samples `from..to` (clamped to what's decoded, whole frames), as plain
    /// interleaved samples.
    pub fn copy(&self, from: usize, to: usize) -> Vec<f32> {
        let to = to.min(self.len()) / 2;
        let from = from.min(to * 2) / 2;
        self.frames[from..to].iter().flat_map(|f| {
            let (l, r) = unpack(f.load(Ordering::Relaxed));
            [l, r]
        }).collect()
    }

    /// What's decoded, as plain samples (for the waveform).
    pub fn to_vec(&self) -> Vec<f32> {
        self.copy(0, self.len())
    }

    /// Store interleaved stereo `samples` as frames from frame `at`; what's
    /// past the end is dropped. Returns the frames stored.
    fn store(&self, at: usize, samples: &[f32]) -> usize {
        let room = self.frames.len().saturating_sub(at);
        let n = (samples.len() / 2).min(room);
        for (k, f) in samples.chunks_exact(2).take(n).enumerate() {
            self.frames[at + k].store(pack(f[0], f[1]), Ordering::Relaxed);
        }
        n
    }
}

/// Every track of a clip, filling in.
pub struct Pcm {
    tracks: Vec<PcmTrack>,
    done: AtomicBool,
}

impl Pcm {
    /// Already decoded tracks (tests, or sound from elsewhere).
    pub fn from_tracks(tracks: Vec<Vec<f32>>) -> Arc<Self> {
        let tracks = tracks
            .into_iter()
            .map(|t| {
                let track = PcmTrack::with_frames(t.len() / 2);
                let n = track.store(0, &t);
                track.len.store(n * 2, Ordering::Release);
                track
            })
            .collect();
        Arc::new(Self { tracks, done: AtomicBool::new(true) })
    }

    pub fn tracks(&self) -> &[PcmTrack] {
        &self.tracks
    }

    pub fn len(&self) -> usize {
        self.tracks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    /// Every track is decoded to its end.
    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }
}

static DECODED: std::sync::Mutex<Vec<Shared>> = std::sync::Mutex::new(Vec::new());

struct Shared {
    path: std::path::PathBuf,
    stamp: crate::Stamp,
    tracks: Vec<usize>,
    pcm: Arc<Pcm>,
    used: std::time::Instant,
}

const KEEP: usize = 2;
const UNUSED: std::time::Duration = std::time::Duration::from_secs(30);

fn shared(source: &Path, tracks: &[usize]) -> Option<Arc<Pcm>> {
    let stamp = crate::stamp(source)?;
    let mut kept = DECODED.lock().ok()?;
    sweep(&mut kept);
    let at = kept.iter().position(|s| s.path == source && s.stamp == stamp && s.tracks == tracks)?;
    kept[at].used = std::time::Instant::now();
    Some(kept[at].pcm.clone())
}

fn keep(source: &Path, tracks: &[usize], pcm: &Arc<Pcm>) {
    let Some(stamp) = crate::stamp(source) else { return };
    let Ok(mut kept) = DECODED.lock() else { return };
    kept.retain(|s| !(s.path == source && s.stamp == stamp && s.tracks == tracks));
    kept.push(Shared { path: source.to_path_buf(), stamp, tracks: tracks.to_vec(), pcm: pcm.clone(), used: std::time::Instant::now() });
    sweep(&mut kept);
}

fn sweep(kept: &mut Vec<Shared>) {
    let now = std::time::Instant::now();
    kept.retain(|s| Arc::strong_count(&s.pcm) > 1 || now.duration_since(s.used) < UNUSED);
    while kept.len() > KEEP {
        let oldest = kept.iter().enumerate().min_by_key(|(_, s)| s.used).map_or(0, |(i, _)| i);
        kept.remove(oldest);
    }
}

/// Start decoding these audio streams (`indices`, as `0:a:<index>`) of a clip
/// `duration` seconds long. Returns at once; `finished` is called (on a
/// decoding thread) when every track is done, or one failed - for a decode
/// this starts: sound that's decoded already needs no finishing.
///
/// One ffmpeg per track, side by side: Windows reads the file once for all of
/// them (they go through it together), and each decodes on its own core —
/// faster to the end than one ffmpeg decoding them all (1.9 s vs 2.6 s for a
/// 2-minute clip on a hard drive), and the start is there as soon.
pub fn decode_streaming(source: &Path, indices: &[usize], duration: f64, finished: impl FnOnce(&Pcm, Result<()>) + Send + 'static) -> Arc<Pcm> {
    if let Some(pcm) = shared(source, indices) {
        return pcm;
    }
    // Room for the whole clip and a little more (the duration is the
    // container's, rounded); anything past it is dropped.
    let frames = ((duration.max(0.0) + 2.0) * PREVIEW_RATE as f64) as usize;
    let pcm = Arc::new(Pcm { tracks: indices.iter().map(|_| PcmTrack::with_frames(frames)).collect(), done: AtomicBool::new(indices.is_empty()) });
    if indices.is_empty() {
        finished(&pcm, Ok(()));
        return pcm;
    }
    keep(source, indices, &pcm);
    let (source, indices, out) = (source.to_path_buf(), indices.to_vec(), pcm.clone());
    DECODING.fetch_add(1, Ordering::Relaxed);
    let cancel = Arc::new(AtomicBool::new(false));
    RUNNING.lock().unwrap().push((source.clone(), cancel.clone()));
    std::thread::Builder::new()
        .name("decode audio".into())
        .spawn(move || {
            let jobs: Vec<_> = indices
                .iter()
                .enumerate()
                .map(|(k, &i)| {
                    let (source, out, cancel) = (source.clone(), out.clone(), cancel.clone());
                    std::thread::spawn(move || decode_track(&source, i, &out.tracks[k], &cancel))
                })
                .collect();
            let mut result = Ok(());
            for job in jobs {
                let r = job.join().unwrap_or_else(|_| Err(anyhow::anyhow!("decoding the sound crashed")));
                if result.is_ok() {
                    result = r;
                }
            }
            out.done.store(true, Ordering::Release);
            RUNNING.lock().unwrap().retain(|(_, c)| !Arc::ptr_eq(c, &cancel));
            DECODING.fetch_sub(1, Ordering::Relaxed);
            finished(&out, result);
        })
        .expect("spawn audio decoder");
    pcm
}

/// Decode one track into `out` as it comes (the same ffmpeg command as
/// [`crate::decode_audio`]).
fn decode_track(source: &Path, index: usize, out: &PcmTrack, cancel: &AtomicBool) -> Result<()> {
    let mut child = crate::ffmpeg()
        .args(["-hide_banner", "-loglevel", "error", "-vn", "-i"])
        .arg(source)
        .args(["-map", &format!("0:a:{index}"), "-ac", "2", "-ar", &PREVIEW_RATE.to_string()])
        .args(["-f", "f32le", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to run ffmpeg")?;
    let mut stdout = child.stdout.take().context("no ffmpeg output")?;
    // ~20 ms of sound at a time; whole stereo frames are published.
    let mut buf = vec![0u8; 8 * 1024];
    let mut samples: Vec<f32> = Vec::with_capacity(buf.len() / 4);
    let (mut have, mut written) = (0usize, 0usize);
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(());
        }
        let read = stdout.read(&mut buf[have..])?;
        if read == 0 {
            break;
        }
        have += read;
        let whole = have / 8 * 8;
        samples.clear();
        samples.extend(buf[..whole].chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])));
        written += out.store(written, &samples);
        out.len.store(written * 2, Ordering::Release);
        buf.copy_within(whole..have, 0);
        have -= whole;
    }
    let status = child.wait()?;
    if !status.success() && written == 0 {
        anyhow::bail!("couldn't decode the sound");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stored sound comes back within half a step, sound over full scale
    /// included, and frames come back in order.
    #[test]
    fn samples_round_trip() {
        let samples: Vec<f32> = (0..2000).map(|i| ((i as f32) * 0.37).sin() * 1.9).chain([1.0, -1.0, 0.0, 1.5]).collect();
        let pcm = Pcm::from_tracks(vec![samples.clone()]);
        let t = &pcm.tracks()[0];
        assert_eq!(t.len(), samples.len());
        let back = t.to_vec();
        let worst = samples.iter().zip(&back).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
        assert!(worst <= 0.5 / SCALE + 1e-7, "{worst}");
        assert_eq!(t.frame(1000), Some((back[2000], back[2001])));
        assert_eq!(t.frame(1002), None, "past what's decoded");
        assert_eq!(t.copy(2, 6), back[2..6]);
    }

    /// A clip with one tone on each of two audio streams.
    fn clip(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("hc-pcm-{name}-{}.mp4", std::process::id()));
        let ok = crate::ffmpeg()
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=64x48:rate=10:duration=2"])
            .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=2"])
            .args(["-f", "lavfi", "-i", "sine=frequency=660:duration=2"])
            .args(["-map", "0", "-map", "1", "-map", "2", "-c:v", "libx264", "-c:a", "aac"])
            .arg(&path)
            .status()
            .unwrap();
        assert!(ok.success());
        path
    }

    fn wait(pcm: &Pcm) {
        for _ in 0..400 {
            if pcm.is_done() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        panic!("the sound never finished decoding");
    }

    /// The same clip's sound, opened again (swapping between the player and
    /// the editor), is the same sound: decoded once, not read again.
    #[test]
    fn the_same_sound_is_shared() {
        let path = clip("shared");
        let first = decode_streaming(&path, &[0], 2.0, |_, _| {});
        wait(&first);
        let second = decode_streaming(&path, &[0], 2.0, |_, _| panic!("decoded again"));
        assert!(Arc::ptr_eq(&first, &second), "the sound was read again");
        assert!(second.is_done() && second.tracks()[0].len() > 0);
        // The editor's tracks are another sound: its own decode.
        let tracks = decode_streaming(&path, &[1], 2.0, |_, _| {});
        assert!(!Arc::ptr_eq(&first, &tracks));
        wait(&tracks);
        let a = first.tracks()[0].to_vec();
        let b = tracks.tracks()[0].to_vec();
        assert!(a.len() > 1000 && (a[1000] - b[1000]).abs() > 0.01, "two tones, not one");
        std::fs::remove_file(&path).unwrap();
    }

    /// A decode cut short (its file is about to be deleted) isn't kept: the
    /// next open reads the file again instead of playing half a clip.
    #[test]
    fn a_cancelled_decode_is_not_kept() {
        let path = clip("cancelled");
        let first = decode_streaming(&path, &[0], 2.0, |_, _| {});
        release(&path);
        let again = decode_streaming(&path, &[0], 2.0, |_, _| {});
        assert!(!Arc::ptr_eq(&first, &again));
        wait(&again);
        std::fs::remove_file(&path).unwrap();
    }
}
