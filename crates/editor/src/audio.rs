use cap_audio::{
    AudioData, AudioRendererTrack, FromSampleBytes, StereoMode, cast_f32_slice_to_bytes,
};
use cap_media::MediaError;
use cap_media_info::AudioInfo;
use cap_project::{AudioConfiguration, ClipOffsets, ProjectConfiguration, TimelineConfiguration};
use ffmpeg::{
    ChannelLayout, Dictionary, format as avformat, frame::Audio as FFAudio, software::resampling,
};
#[cfg(not(target_os = "windows"))]
use ringbuf::{
    HeapRb,
    traits::{Consumer, Observer, Producer},
};
use std::sync::Arc;
use tracing::info;

pub struct AudioRenderer {
    data: Vec<AudioSegment>,
    music: Option<Arc<AudioData>>,
    cursor: AudioRendererCursor,
    // sum of `frame.samples()` that have elapsed
    // this * channel count = cursor
    elapsed_samples: usize,
}

#[derive(Clone, Copy, Debug)]
pub struct AudioRendererCursor {
    clip_index: u32,
    timescale: f64,
    // excludes channels
    samples: usize,
}

#[derive(Clone)]
pub struct AudioSegment {
    pub tracks: Vec<AudioSegmentTrack>,
}

// yeah this is cursed oh well
#[derive(Clone)]
pub struct AudioSegmentTrack {
    data: Arc<AudioData>,
    get_gain: fn(&AudioConfiguration) -> f32,
    get_stereo_mode: fn(&AudioConfiguration) -> StereoMode,
    get_offset: fn(&ClipOffsets) -> f32,
}

impl AudioSegmentTrack {
    pub fn new(
        data: Arc<AudioData>,
        get_gain: fn(&AudioConfiguration) -> f32,
        get_stereo_mode: fn(&AudioConfiguration) -> StereoMode,
        get_offset: fn(&ClipOffsets) -> f32,
    ) -> Self {
        Self {
            data,
            get_gain,
            get_stereo_mode,
            get_offset,
        }
    }

    pub fn data(&self) -> &Arc<AudioData> {
        &self.data
    }

    pub fn gain(&self, config: &AudioConfiguration) -> f32 {
        (self.get_gain)(config)
    }

    pub fn stereo_mode(&self, config: &AudioConfiguration) -> StereoMode {
        (self.get_stereo_mode)(config)
    }

    pub fn offset(&self, offsets: &ClipOffsets) -> f32 {
        (self.get_offset)(offsets)
    }
}

struct TimelineCursor<'a> {
    segment_end_samples: usize,
    segment_time: f64,
    segment: &'a cap_project::TimelineSegment,
}

impl AudioRenderer {
    pub const SAMPLE_FORMAT: avformat::Sample = AudioData::SAMPLE_FORMAT;
    pub const SAMPLE_RATE: u32 = AudioData::SAMPLE_RATE;
    pub const CHANNELS: u16 = 2;

    pub fn info() -> AudioInfo {
        AudioInfo::new(Self::SAMPLE_FORMAT, Self::SAMPLE_RATE, Self::CHANNELS).unwrap()
    }

    pub fn new(data: Vec<AudioSegment>, music: Option<Arc<AudioData>>) -> Self {
        Self {
            data,
            music,
            cursor: AudioRendererCursor {
                clip_index: 0,
                samples: 0,
                timescale: 1.0,
            },
            elapsed_samples: 0,
        }
    }

    pub fn set_playhead(&mut self, playhead: f64, project: &ProjectConfiguration) {
        self.elapsed_samples = self.playhead_to_samples(playhead);

        self.cursor = match project.get_segment_time(playhead) {
            Some((segment_time, segment)) => AudioRendererCursor {
                clip_index: segment.recording_clip,
                timescale: segment.timescale,
                samples: self.playhead_to_samples(segment_time),
            },
            None => AudioRendererCursor {
                clip_index: 0,
                timescale: 1.0,
                samples: self.elapsed_samples,
            },
        };
    }

    fn playhead_to_samples(&self, playhead: f64) -> usize {
        (playhead * AudioData::SAMPLE_RATE as f64).round() as usize
    }

    pub fn elapsed_samples_to_playhead(&self) -> f64 {
        self.elapsed_samples as f64 / AudioData::SAMPLE_RATE as f64
    }

    pub fn render_frame(
        &mut self,
        requested_samples: usize,
        project: &ProjectConfiguration,
    ) -> Option<FFAudio> {
        self.render_frame_raw(requested_samples, project)
            .map(move |(samples, data)| {
                let mut raw_frame =
                    FFAudio::new(AudioData::SAMPLE_FORMAT, samples, ChannelLayout::STEREO);
                raw_frame.set_rate(AudioData::SAMPLE_RATE);
                raw_frame.data_mut(0)[0..data.len() * f32::BYTE_SIZE]
                    .copy_from_slice(unsafe { cast_f32_slice_to_bytes(&data) });

                raw_frame
            })
    }

    pub fn render_frame_raw(
        &mut self,
        samples: usize,
        project: &ProjectConfiguration,
    ) -> Option<(usize, Vec<f32>)> {
        if let Some(timeline) = &project.timeline {
            return self.render_timeline_frame_raw(samples, project, timeline);
        }

        self.render_linear_frame_raw(samples, project)
    }

    fn render_timeline_frame_raw(
        &mut self,
        samples: usize,
        project: &ProjectConfiguration,
        timeline: &TimelineConfiguration,
    ) -> Option<(usize, Vec<f32>)> {
        if samples == 0 {
            return None;
        }

        let mut ret = vec![0.0; samples * 2];
        let mut written = 0usize;
        let start_elapsed = self.elapsed_samples;

        while written < samples {
            let Some(cursor) = self.timeline_cursor(timeline) else {
                break;
            };

            let chunk_samples =
                (cursor.segment_end_samples - self.elapsed_samples).min(samples - written);
            if chunk_samples == 0 {
                break;
            }

            self.cursor = AudioRendererCursor {
                clip_index: cursor.segment.recording_clip,
                timescale: cursor.segment.timescale,
                samples: self.playhead_to_samples(cursor.segment_time),
            };

            if cursor.segment.timescale == 1.0 {
                self.render_current_chunk(project, chunk_samples, written * 2, &mut ret);
                self.cursor.samples += chunk_samples;
            }

            self.elapsed_samples += chunk_samples;
            written += chunk_samples;
        }

        if written == 0 {
            None
        } else {
            ret.truncate(written * 2);
            self.mix_music(project, start_elapsed, written, &mut ret);
            Some((written, ret))
        }
    }

    fn render_linear_frame_raw(
        &mut self,
        samples: usize,
        project: &ProjectConfiguration,
    ) -> Option<(usize, Vec<f32>)> {
        if samples == 0 {
            return None;
        }

        if self.cursor.timescale != 1.0 {
            self.elapsed_samples += samples;
            return None;
        }

        let mut ret = vec![0.0; samples * 2];
        let rendered = self.render_current_chunk(project, samples, 0, &mut ret);

        if rendered == 0 {
            self.elapsed_samples += samples;
            return None;
        }

        let start_elapsed = self.elapsed_samples;
        self.elapsed_samples += rendered;
        self.cursor.samples += rendered;
        ret.truncate(rendered * 2);
        self.mix_music(project, start_elapsed, rendered, &mut ret);

        Some((rendered, ret))
    }

    fn timeline_cursor<'a>(
        &self,
        timeline: &'a TimelineConfiguration,
    ) -> Option<TimelineCursor<'a>> {
        let mut segment_start_samples = 0usize;
        let mut accumulated_duration = 0.0;

        for segment in &timeline.segments {
            accumulated_duration += segment.duration();
            let segment_end_samples = self.playhead_to_samples(accumulated_duration);

            if self.elapsed_samples < segment_end_samples {
                let local_samples = self.elapsed_samples - segment_start_samples;
                let local_time = local_samples as f64 / Self::SAMPLE_RATE as f64;
                return Some(TimelineCursor {
                    segment_end_samples,
                    segment_time: segment.start + local_time * segment.timescale,
                    segment,
                });
            }

            segment_start_samples = segment_end_samples;
        }

        None
    }

    fn mix_music(
        &self,
        project: &ProjectConfiguration,
        start_frame: usize,
        frames: usize,
        out: &mut [f32],
    ) {
        let Some(music) = &self.music else {
            return;
        };
        if project.audio.mute {
            return;
        }
        cap_audio::mix_looped_track(
            music,
            project.audio.music_volume_db,
            start_frame,
            frames,
            0,
            out,
        );
    }

    fn render_current_chunk(
        &self,
        project: &ProjectConfiguration,
        samples: usize,
        out_offset: usize,
        out: &mut [f32],
    ) -> usize {
        let Some(segment) = self.data.get(self.cursor.clip_index as usize) else {
            return 0;
        };
        let tracks = &segment.tracks;

        if tracks.is_empty() {
            return 0;
        }

        let offsets = project
            .clips
            .iter()
            .find(|c| c.index == self.cursor.clip_index)
            .map(|c| c.offsets)
            .unwrap_or_default();

        let max_samples = tracks
            .iter()
            .map(|t| {
                let track_offset_samples =
                    (t.offset(&offsets) * Self::SAMPLE_RATE as f32).round() as isize;
                let available = t.data().sample_count() as isize - track_offset_samples;
                available.max(0) as usize
            })
            .max()
            .unwrap_or(0);

        if self.cursor.samples >= max_samples {
            return 0;
        }

        let samples = samples.min(max_samples - self.cursor.samples);

        let track_datas = tracks
            .iter()
            .map(|t| AudioRendererTrack {
                data: t.data().as_ref(),
                gain: if project.audio.mute {
                    f32::NEG_INFINITY
                } else {
                    let g = t.gain(&project.audio);
                    if g < -30.0 { f32::NEG_INFINITY } else { g }
                },
                stereo_mode: t.stereo_mode(&project.audio),
                offset: (t.offset(&offsets) * Self::SAMPLE_RATE as f32).round() as isize,
            })
            .collect::<Vec<_>>();

        cap_audio::render_audio(&track_datas, self.cursor.samples, samples, out_offset, out)
    }
}

#[cfg(not(target_os = "windows"))]
pub struct AudioPlaybackBuffer<T: FromSampleBytes> {
    frame_buffer: AudioRenderer,
    resampler: AudioResampler,
    resampled_buffer: HeapRb<T>,
}

#[cfg(not(target_os = "windows"))]
impl<T: FromSampleBytes> AudioPlaybackBuffer<T> {
    pub const PLAYBACK_SAMPLES_COUNT: u32 = 512;

    pub const WIRELESS_PLAYBACK_SAMPLES_COUNT: u32 = 1024;

    const PROCESSING_SAMPLES_COUNT: u32 = 1024;

    pub fn new(
        data: Vec<AudioSegment>,
        music: Option<Arc<AudioData>>,
        output_info: AudioInfo,
    ) -> Self {
        // Clamp output info for FFmpeg compatibility (max 8 channels)
        let output_info = output_info.for_ffmpeg_output();

        info!(
            sample_rate = output_info.sample_rate,
            channels = output_info.channels,
            sample_format = ?output_info.sample_format,
            "Audio playback output configuration"
        );

        let resampler = AudioResampler::new(output_info).unwrap();

        let capacity = (output_info.sample_rate as usize)
            * output_info.channels
            * output_info.sample_format.bytes();
        let resampled_buffer = HeapRb::new(capacity);

        let frame_buffer = AudioRenderer::new(data, music);

        Self {
            frame_buffer,
            resampler,
            resampled_buffer,
        }
    }

    pub fn set_playhead(&mut self, playhead: f64, project: &ProjectConfiguration) {
        self.resampler.reset();
        self.resampled_buffer.clear();
        self.frame_buffer.set_playhead(playhead, project);
    }

    #[allow(dead_code)]
    pub fn current_playhead(&self) -> f64 {
        self.frame_buffer.elapsed_samples_to_playhead()
    }

    pub fn current_audible_playhead(
        &self,
        device_sample_rate: u32,
        device_latency_secs: f64,
    ) -> f64 {
        let generated_secs = self.frame_buffer.elapsed_samples_to_playhead();
        let channels = self.resampler.output.channels;
        let buffered_elements = self.resampled_buffer.occupied_len();
        let buffered_frames = buffered_elements / channels;
        let buffered_secs = buffered_frames as f64 / device_sample_rate as f64;
        let audible = generated_secs - buffered_secs - device_latency_secs.max(0.0);
        if audible.is_sign_negative() {
            0.0
        } else {
            audible
        }
    }

    pub fn buffer_reaching_limit(&self) -> bool {
        self.resampled_buffer.vacant_len()
            <= 2 * (Self::PROCESSING_SAMPLES_COUNT as usize) * self.resampler.output.channels
    }

    fn render_chunk(&mut self, project: &ProjectConfiguration) -> bool {
        if self.buffer_reaching_limit() {
            return false;
        }

        let bytes_per_sample = self.resampler.output.sample_size();

        let next_frame = self
            .frame_buffer
            .render_frame(Self::PROCESSING_SAMPLES_COUNT as usize, project);

        let maybe_rendered = match next_frame {
            Some(frame) => Some(self.resampler.queue_and_process_frame(&frame)),
            None => self.resampler.flush_frame(),
        };

        let Some(rendered) = maybe_rendered else {
            return false;
        };

        if rendered.is_empty() {
            return false;
        }

        let mut typed_data = vec![T::EQUILIBRIUM; rendered.len() / bytes_per_sample];

        for (src, dest) in std::iter::zip(rendered.chunks(bytes_per_sample), &mut typed_data) {
            *dest = T::from_bytes(src);
        }
        self.resampled_buffer.push_slice(&typed_data);
        true
    }

    pub fn prefill(&mut self, project: &ProjectConfiguration, min_samples: usize) {
        if min_samples == 0 {
            return;
        }

        let capacity = self.resampled_buffer.capacity().get();
        let target = min_samples.min(capacity);

        while self.resampled_buffer.occupied_len() < target {
            if !self.render_chunk(project) {
                break;
            }
        }
    }

    pub fn fill(
        &mut self,
        playback_buffer: &mut [T],
        project: &ProjectConfiguration,
        min_headroom_samples: usize,
    ) {
        let filled = self.resampled_buffer.pop_slice(playback_buffer);
        playback_buffer[filled..].fill(T::EQUILIBRIUM);

        self.prefill(project, min_headroom_samples);
    }
}

pub struct AudioResampler {
    pub context: resampling::Context,
    pub output_frame: FFAudio,
    delay: Option<resampling::Delay>,
    output: AudioInfo,
}

impl AudioResampler {
    pub fn new(output_info: AudioInfo) -> Result<Self, MediaError> {
        // Clamp output info for FFmpeg compatibility (max 8 channels)
        let output_info = output_info.for_ffmpeg_output();

        let mut options = Dictionary::new();
        options.set("filter_size", "128");
        options.set("cutoff", "0.97");

        let context = resampling::Context::get_with(
            AudioData::SAMPLE_FORMAT,
            ChannelLayout::STEREO,
            AudioData::SAMPLE_RATE,
            output_info.sample_format,
            output_info.channel_layout(),
            output_info.sample_rate,
            options,
        )?;

        info!(
            input_rate = AudioData::SAMPLE_RATE,
            output_rate = output_info.sample_rate,
            output_format = ?output_info.sample_format,
            "Audio resampler created with high-quality settings (filter_size=128)"
        );

        Ok(Self {
            output: output_info,
            context,
            output_frame: FFAudio::empty(),
            delay: None,
        })
    }

    #[cfg(not(target_os = "windows"))]
    pub fn reset(&mut self) {
        *self = Self::new(self.output).unwrap();
    }

    fn current_frame_data(&self) -> &[u8] {
        let end = self.output_frame.samples() * self.output.channels * self.output.sample_size();
        &self.output_frame.data(0)[0..end]
    }

    pub fn queue_and_process_frame<'a>(&'a mut self, frame: &FFAudio) -> &'a [u8] {
        self.delay = self.context.run(frame, &mut self.output_frame).unwrap();

        // Teeechnically this doesn't work for planar output
        self.current_frame_data()
    }

    pub fn flush_frame(&mut self) -> Option<&[u8]> {
        self.delay?;

        self.delay = self.context.flush(&mut self.output_frame).unwrap();

        Some(self.current_frame_data())
    }
}

pub struct PrerenderedAudioBuffer<T: FromSampleBytes> {
    // Main mix (mic + system) at device rate, f32 so the live-gain music mix
    // below stays lossless until the final sample-format conversion.
    samples: Vec<f32>,
    // One loop of the background music at device rate. Kept out of the main
    // mix so its gain can follow the volume slider live during playback.
    music_samples: Vec<f32>,
    read_position: usize,
    sample_rate: u32,
    channels: usize,
    _format: std::marker::PhantomData<T>,
}

impl<T: FromSampleBytes> PrerenderedAudioBuffer<T> {
    pub fn new(
        segments: Vec<AudioSegment>,
        music: Option<Arc<AudioData>>,
        project: &ProjectConfiguration,
        output_info: AudioInfo,
        duration_secs: f64,
    ) -> Self {
        // Clamp output info for FFmpeg compatibility (max 8 channels)
        // The resampler will produce audio with this channel count
        let output_info = output_info.for_ffmpeg_output();
        let mut f32_output_info = output_info;
        f32_output_info.sample_format = AudioData::SAMPLE_FORMAT;

        info!(
            duration_secs = duration_secs,
            sample_rate = output_info.sample_rate,
            channels = output_info.channels,
            "Pre-rendering audio for playback"
        );

        let mut renderer = AudioRenderer::new(segments, None);
        let mut resampler = AudioResampler::new(f32_output_info).unwrap();

        let total_source_samples = (duration_secs * AudioData::SAMPLE_RATE as f64) as usize;
        let estimated_output_samples =
            (duration_secs * output_info.sample_rate as f64) as usize * output_info.channels;

        let mut samples: Vec<f32> = Vec::with_capacity(estimated_output_samples + 10000);
        let bytes_per_sample = f32_output_info.sample_size();
        let chunk_size = 1024usize;

        renderer.set_playhead(0.0, project);

        let mut rendered_source_samples = 0usize;
        let output_chunk_samples = (chunk_size as f64 * output_info.sample_rate as f64
            / AudioData::SAMPLE_RATE as f64) as usize
            * output_info.channels;

        while rendered_source_samples < total_source_samples {
            let frame_opt = renderer.render_frame(chunk_size, project);

            match frame_opt {
                Some(frame) => {
                    let resampled = resampler.queue_and_process_frame(&frame);
                    for chunk in resampled.chunks(bytes_per_sample) {
                        samples.push(f32::from_bytes(chunk));
                    }
                }
                None => {
                    if let Some(flushed) = resampler.flush_frame() {
                        for chunk in flushed.chunks(bytes_per_sample) {
                            samples.push(f32::from_bytes(chunk));
                        }
                    }
                    samples.resize(samples.len() + output_chunk_samples, 0.0);
                }
            }

            rendered_source_samples += chunk_size;
        }

        while let Some(flushed) = resampler.flush_frame() {
            if flushed.is_empty() {
                break;
            }
            for chunk in flushed.chunks(bytes_per_sample) {
                samples.push(f32::from_bytes(chunk));
            }
        }

        let music_samples = music
            .map(|music| prerender_music_stem(&music, f32_output_info))
            .unwrap_or_default();

        info!(
            total_samples = samples.len(),
            music_samples = music_samples.len(),
            memory_mb = ((samples.len() + music_samples.len()) * std::mem::size_of::<f32>())
                / (1024 * 1024),
            "Audio pre-rendering complete"
        );

        Self {
            samples,
            music_samples,
            read_position: 0,
            sample_rate: output_info.sample_rate,
            channels: output_info.channels,
            _format: std::marker::PhantomData,
        }
    }

    pub fn set_playhead(&mut self, playhead_secs: f64) {
        let sample_position = (playhead_secs * self.sample_rate as f64) as usize * self.channels;
        self.read_position = sample_position.min(self.samples.len());
    }

    pub fn current_audible_playhead(&self, device_latency_secs: f64) -> f64 {
        let generated_secs = (self.read_position / self.channels) as f64 / self.sample_rate as f64;
        (generated_secs - device_latency_secs.max(0.0)).max(0.0)
    }

    #[allow(dead_code)]
    pub fn current_playhead_secs(&self) -> f64 {
        (self.read_position / self.channels) as f64 / self.sample_rate as f64
    }

    pub fn fill(&mut self, buffer: &mut [T], music_gain_db: f32)
    where
        T: cpal::Sample + cpal::FromSample<f32>,
    {
        let available = self.samples.len().saturating_sub(self.read_position);
        let to_copy = buffer.len().min(available);

        let gain = cap_audio::gain_for_db(music_gain_db);
        let music = (!self.music_samples.is_empty() && gain != f32::NEG_INFINITY)
            .then_some(&self.music_samples);

        for (i, out) in buffer.iter_mut().enumerate().take(to_copy) {
            let mut value = self.samples[self.read_position + i];
            if let Some(music) = music {
                let music_sample = music[(self.read_position + i) % music.len()];
                value = (value + music_sample * gain).clamp(-1.0, 1.0);
            }
            *out = T::from_sample(value);
        }
        self.read_position += to_copy;

        if to_copy < buffer.len() {
            buffer[to_copy..].fill(T::EQUILIBRIUM);
        }
    }
}

// Resample one full pass of the music (48kHz stereo from AudioData) to the
// device output rate/channel-count so `fill` can loop it by index. Mono
// sources are widened to stereo first because the resampler input layout is
// fixed to stereo.
fn prerender_music_stem(music: &AudioData, f32_output_info: AudioInfo) -> Vec<f32> {
    let stereo: Vec<f32> = if music.channels() == 1 {
        music
            .samples()
            .iter()
            .flat_map(|s| {
                let v = s * 0.707;
                [v, v]
            })
            .collect()
    } else {
        music.samples().to_vec()
    };

    let mut resampler = match AudioResampler::new(f32_output_info) {
        Ok(resampler) => resampler,
        Err(e) => {
            tracing::warn!(?e, "Failed to create music resampler, skipping music");
            return Vec::new();
        }
    };

    let bytes_per_sample = f32_output_info.sample_size();
    let mut out = Vec::with_capacity(
        stereo.len() / 2 * f32_output_info.channels * f32_output_info.sample_rate as usize
            / AudioData::SAMPLE_RATE as usize
            + 10000,
    );

    const CHUNK_FRAMES: usize = 4096;
    for chunk in stereo.chunks(CHUNK_FRAMES * 2) {
        let frames = chunk.len() / 2;
        let mut frame = FFAudio::new(AudioData::SAMPLE_FORMAT, frames, ChannelLayout::STEREO);
        frame.set_rate(AudioData::SAMPLE_RATE);
        frame.data_mut(0)[0..chunk.len() * f32::BYTE_SIZE]
            .copy_from_slice(unsafe { cast_f32_slice_to_bytes(chunk) });

        let resampled = resampler.queue_and_process_frame(&frame);
        for bytes in resampled.chunks(bytes_per_sample) {
            out.push(f32::from_bytes(bytes));
        }
    }

    while let Some(flushed) = resampler.flush_frame() {
        if flushed.is_empty() {
            break;
        }
        for bytes in flushed.chunks(bytes_per_sample) {
            out.push(f32::from_bytes(bytes));
        }
    }

    // Keep whole frames only so looping by index stays channel-aligned.
    out.truncate(out.len() - out.len() % f32_output_info.channels);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use cap_project::{
        ClipConfiguration, ProjectConfiguration, TimelineConfiguration, TimelineSegment,
    };
    use std::{path::Path, sync::Arc};
    use tempfile::TempDir;

    fn gain(_: &AudioConfiguration) -> f32 {
        0.0
    }

    fn stereo(_: &AudioConfiguration) -> StereoMode {
        StereoMode::Stereo
    }

    fn no_offset(_: &ClipOffsets) -> f32 {
        0.0
    }

    fn write_step_wav(path: &Path, section_values: &[i16]) {
        let sample_rate = AudioData::SAMPLE_RATE;
        let channels = 2u16;
        let bits_per_sample = 16u16;
        let section_frames = sample_rate as usize;
        let total_frames = section_frames * section_values.len();
        let bytes_per_frame = usize::from(channels) * usize::from(bits_per_sample / 8);
        let data_size = total_frames * bytes_per_frame;
        let mut bytes = Vec::with_capacity(44 + data_size);

        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_size as u32).to_le_bytes());
        bytes.extend_from_slice(b"WAVE");
        bytes.extend_from_slice(b"fmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&channels.to_le_bytes());
        bytes.extend_from_slice(&sample_rate.to_le_bytes());
        bytes.extend_from_slice(&(sample_rate * bytes_per_frame as u32).to_le_bytes());
        bytes.extend_from_slice(&(bytes_per_frame as u16).to_le_bytes());
        bytes.extend_from_slice(&bits_per_sample.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&(data_size as u32).to_le_bytes());

        for value in section_values {
            for _ in 0..section_frames {
                bytes.extend_from_slice(&value.to_le_bytes());
                bytes.extend_from_slice(&value.to_le_bytes());
            }
        }

        std::fs::write(path, bytes).unwrap();
    }

    fn mean_abs(samples: &[f32]) -> f32 {
        samples.iter().map(|sample| sample.abs()).sum::<f32>() / samples.len() as f32
    }

    fn build_renderer_fixture() -> (TempDir, AudioRenderer, ProjectConfiguration) {
        let _ = ffmpeg::init();

        let dir = tempfile::tempdir().unwrap();
        let clip0_path = dir.path().join("clip0.wav");
        let clip1_path = dir.path().join("clip1.wav");

        write_step_wav(&clip0_path, &[1000, 2000, 3000]);
        write_step_wav(&clip1_path, &[4000, 5000, 6000]);

        let segments = vec![
            AudioSegment {
                tracks: vec![AudioSegmentTrack::new(
                    Arc::new(AudioData::from_file(&clip0_path).unwrap()),
                    gain,
                    stereo,
                    no_offset,
                )],
            },
            AudioSegment {
                tracks: vec![AudioSegmentTrack::new(
                    Arc::new(AudioData::from_file(&clip1_path).unwrap()),
                    gain,
                    stereo,
                    no_offset,
                )],
            },
        ];

        let project = ProjectConfiguration {
            timeline: Some(TimelineConfiguration {
                segments: vec![
                    TimelineSegment {
                        recording_clip: 0,
                        timescale: 1.0,
                        start: 0.0,
                        end: 1.0,
                    },
                    TimelineSegment {
                        recording_clip: 0,
                        timescale: 4.0,
                        start: 1.0,
                        end: 2.0,
                    },
                    TimelineSegment {
                        recording_clip: 0,
                        timescale: 1.0,
                        start: 2.0,
                        end: 3.0,
                    },
                    TimelineSegment {
                        recording_clip: 1,
                        timescale: 1.0,
                        start: 0.0,
                        end: 1.0,
                    },
                    TimelineSegment {
                        recording_clip: 1,
                        timescale: 2.0,
                        start: 1.0,
                        end: 2.0,
                    },
                    TimelineSegment {
                        recording_clip: 1,
                        timescale: 1.0,
                        start: 2.0,
                        end: 3.0,
                    },
                ],
                zoom_segments: Vec::new(),
                scene_segments: Vec::new(),
                mask_segments: Vec::new(),
                text_segments: Vec::new(),
                caption_segments: Vec::new(),
                keyboard_segments: Vec::new(),
            }),
            clips: vec![
                ClipConfiguration {
                    index: 0,
                    offsets: Default::default(),
                },
                ClipConfiguration {
                    index: 1,
                    offsets: Default::default(),
                },
            ],
            ..Default::default()
        };

        (dir, AudioRenderer::new(segments, None), project)
    }

    #[test]
    fn prerendered_audio_reports_audible_playhead_after_output_latency() {
        let mut buffer = PrerenderedAudioBuffer::<f32> {
            samples: vec![0.0; AudioData::SAMPLE_RATE as usize * 2],
            music_samples: Vec::new(),
            read_position: 0,
            sample_rate: AudioData::SAMPLE_RATE,
            channels: 2,
            _format: std::marker::PhantomData,
        };

        buffer.set_playhead(0.5);

        assert!((buffer.current_audible_playhead(0.2) - 0.3).abs() < 0.000_1);
        assert_eq!(buffer.current_audible_playhead(1.0), 0.0);
    }

    // The music stem is mixed at fill time so the volume slider applies live,
    // without re-rendering the prerendered main mix.
    #[test]
    fn prerendered_fill_mixes_music_with_live_gain() {
        let mut buffer = PrerenderedAudioBuffer::<f32> {
            samples: vec![0.1; 8],
            music_samples: vec![0.5, 0.5],
            read_position: 0,
            sample_rate: AudioData::SAMPLE_RATE,
            channels: 2,
            _format: std::marker::PhantomData,
        };

        let mut out = vec![0.0f32; 4];
        buffer.fill(&mut out, 0.0);
        assert!((out[0] - 0.6).abs() < 1e-6);
        assert!((out[3] - 0.6).abs() < 1e-6);

        let mut out = vec![0.0f32; 4];
        buffer.fill(&mut out, -30.0);
        assert!((out[0] - 0.1).abs() < 1e-6);

        // Past the main mix the buffer pads silence; music must not extend it.
        let mut out = vec![0.7f32; 4];
        buffer.fill(&mut out, 0.0);
        assert_eq!(out, vec![0.0; 4]);
    }

    #[test]
    fn speed_segment_start_cuts_audio_inside_a_single_request() {
        let (_dir, mut renderer, project) = build_renderer_fixture();
        let boundary = 1.0 + 0.25 + 1.0 + 1.0;

        renderer.set_playhead(boundary - 0.01, &project);

        let (rendered, samples) = renderer.render_frame_raw(1920, &project).unwrap();
        assert_eq!(rendered, 1920);

        let boundary_samples = (0.01 * AudioData::SAMPLE_RATE as f64) as usize;
        let before = mean_abs(&samples[..boundary_samples * 2]);
        let after = mean_abs(&samples[boundary_samples * 2..]);

        assert!(before > 0.1);
        assert!(after < 0.0001);
    }

    #[test]
    fn speed_segment_end_resumes_audio_inside_a_single_request() {
        let (_dir, mut renderer, project) = build_renderer_fixture();
        let boundary = 1.0 + 0.25 + 1.0 + 1.0 + 0.5;

        renderer.set_playhead(boundary - 0.01, &project);

        let (rendered, samples) = renderer.render_frame_raw(1920, &project).unwrap();
        assert_eq!(rendered, 1920);

        let boundary_samples = (0.01 * AudioData::SAMPLE_RATE as f64) as usize;
        let before = mean_abs(&samples[..boundary_samples * 2]);
        let after = mean_abs(&samples[boundary_samples * 2..]);

        assert!(before < 0.0001);
        assert!(after > 0.15);
    }

    /// One clip per second `section_values`, on a timeline made of `segments`.
    fn single_clip_fixture(
        section_values: &[i16],
        segments: Vec<TimelineSegment>,
    ) -> (TempDir, AudioRenderer, ProjectConfiguration) {
        let _ = ffmpeg::init();

        let dir = tempfile::tempdir().unwrap();
        let clip_path = dir.path().join("clip.wav");
        write_step_wav(&clip_path, section_values);

        let data = vec![AudioSegment {
            tracks: vec![AudioSegmentTrack::new(
                Arc::new(AudioData::from_file(&clip_path).unwrap()),
                gain,
                stereo,
                no_offset,
            )],
        }];

        let project = ProjectConfiguration {
            timeline: Some(TimelineConfiguration {
                segments,
                zoom_segments: Vec::new(),
                scene_segments: Vec::new(),
                mask_segments: Vec::new(),
                text_segments: Vec::new(),
                caption_segments: Vec::new(),
                keyboard_segments: Vec::new(),
            }),
            clips: vec![ClipConfiguration {
                index: 0,
                offsets: Default::default(),
            }],
            ..Default::default()
        };

        (dir, AudioRenderer::new(data, None), project)
    }

    fn segment(recording_clip: u32, start: f64, end: f64, timescale: f64) -> TimelineSegment {
        TimelineSegment {
            recording_clip,
            timescale,
            start,
            end,
        }
    }

    /// Mirrors the export encoder loop in `crates/export/src/mp4.rs`: seed the
    /// playhead once at 0.0, then render `((n+1)*sr)/fps - cursor` samples per
    /// output frame. Returns the interleaved-stereo stream, padded so that output
    /// sample index `j` maps to output presentation time `j / sr`.
    fn render_export_audio(
        renderer: &mut AudioRenderer,
        project: &ProjectConfiguration,
        fps: u64,
        frames: u64,
    ) -> Vec<f32> {
        let sr = u64::from(AudioData::SAMPLE_RATE);
        renderer.set_playhead(0.0, project);

        let mut cursor = 0u64;
        let mut out = Vec::new();
        for n in 0..frames {
            let end = ((n + 1) * sr) / fps;
            if end <= cursor {
                continue;
            }
            let budget = (end - cursor) as usize;
            cursor = end;

            let mut chunk = renderer
                .render_frame_raw(budget, project)
                .map(|(_, samples)| samples)
                .unwrap_or_default();
            chunk.resize(budget * 2, 0.0);
            out.extend(chunk);
        }
        out
    }

    /// Left channel value at the middle of output second `out_second`. The fixture
    /// holds a constant value per source second, so this reveals which source
    /// sample the export read for that presentation time.
    fn left_at_second(stream: &[f32], out_second: usize) -> f32 {
        let mid = (out_second * AudioData::SAMPLE_RATE as usize
            + AudioData::SAMPLE_RATE as usize / 2)
            * 2;
        stream[mid]
    }

    fn expected(value: i16) -> f32 {
        value as f32 / 32768.0
    }

    // Time->sample conversion rounds to nearest (not truncates), so a fractional
    // sample position lands on the nearest sample rather than biasing downward.
    #[test]
    fn playhead_to_samples_rounds_to_nearest() {
        let renderer = AudioRenderer::new(vec![], None);
        let sr = AudioData::SAMPLE_RATE as f64;
        // 5.7 samples of time -> 6 (rounded); truncation would give 5.
        assert_eq!(renderer.playhead_to_samples(5.7 / sr), 6);
        // 5.2 samples of time -> 5 (rounded down).
        assert_eq!(renderer.playhead_to_samples(5.2 / sr), 5);
    }

    // Invariant: with a full, untrimmed, 1.0-timescale segment the exported audio
    // reads the source 1:1 — output presentation time T contains source audio at
    // time T — and this holds identically across every fps (no fps-dependent
    // positional shift).
    #[test]
    fn export_audio_tracks_presentation_time_across_fps() {
        let values = [3000i16, 6000, 9000, 12000, 15000];
        for fps in [24u64, 30, 60] {
            let (_dir, mut renderer, project) =
                single_clip_fixture(&values, vec![segment(0, 0.0, 5.0, 1.0)]);
            let stream = render_export_audio(&mut renderer, &project, fps, 5 * fps);

            for (sec, value) in values.iter().enumerate() {
                let got = left_at_second(&stream, sec);
                assert!(
                    (got - expected(*value)).abs() < 0.01,
                    "fps {fps} second {sec}: read {got}, expected {}",
                    expected(*value)
                );
            }
        }
    }

    // Invariant #1: a trimmed segment (start = 2.0s) must offset the audio read
    // position by the trim, exactly like the video timeline mapping.
    #[test]
    fn export_audio_honors_timeline_trim_offset() {
        let values = [3000i16, 6000, 9000, 12000, 15000];
        let (_dir, mut renderer, project) =
            single_clip_fixture(&values, vec![segment(0, 2.0, 5.0, 1.0)]);
        let stream = render_export_audio(&mut renderer, &project, 30, 3 * 30);

        // Output second k reads source second 2 + k.
        for out_second in 0..3usize {
            let got = left_at_second(&stream, out_second);
            let want = expected(values[2 + out_second]);
            assert!(
                (got - want).abs() < 0.01,
                "out second {out_second}: read {got}, expected {want}"
            );
        }
    }

    // Background music is anchored to the OUTPUT timeline: a trimmed clip shifts
    // what the mic track reads, but music still starts at output 0:00 and loops
    // past its own length for the full video.
    #[test]
    fn background_music_loops_and_follows_output_timeline() {
        let _ = ffmpeg::init();

        let dir = tempfile::tempdir().unwrap();
        let clip_path = dir.path().join("clip.wav");
        let music_path = dir.path().join("music.wav");
        write_step_wav(&clip_path, &[3000, 6000, 9000]);
        write_step_wav(&music_path, &[8000]);

        let data = vec![AudioSegment {
            tracks: vec![AudioSegmentTrack::new(
                Arc::new(AudioData::from_file(&clip_path).unwrap()),
                gain,
                stereo,
                no_offset,
            )],
        }];
        let music = Arc::new(AudioData::from_file(&music_path).unwrap());

        let project = ProjectConfiguration {
            timeline: Some(TimelineConfiguration {
                segments: vec![segment(0, 1.0, 3.0, 1.0)],
                zoom_segments: Vec::new(),
                scene_segments: Vec::new(),
                mask_segments: Vec::new(),
                text_segments: Vec::new(),
                caption_segments: Vec::new(),
                keyboard_segments: Vec::new(),
            }),
            clips: vec![ClipConfiguration {
                index: 0,
                offsets: Default::default(),
            }],
            ..Default::default()
        };

        let mut renderer = AudioRenderer::new(data, Some(music));
        let stream = render_export_audio(&mut renderer, &project, 30, 2 * 30);

        // Trim start 1.0 -> output second 0 reads clip second 1 (6000); music
        // contributes from output 0 and, being 1s long, loops into second 1.
        let want_sec0 = expected(6000) + expected(8000);
        let want_sec1 = expected(9000) + expected(8000);
        assert!((left_at_second(&stream, 0) - want_sec0).abs() < 0.01);
        assert!((left_at_second(&stream, 1) - want_sec1).abs() < 0.01);
    }

    // Invariant #1: a multi-segment jump cut must re-anchor the audio read at the
    // boundary (no carry-over from segment 0), keeping audio aligned to the cut.
    #[test]
    fn export_audio_reanchors_across_segment_boundary() {
        let values = [3000i16, 6000, 9000, 12000, 15000];
        let (_dir, mut renderer, project) = single_clip_fixture(
            &values,
            vec![segment(0, 0.0, 1.0, 1.0), segment(0, 3.0, 4.0, 1.0)],
        );
        let stream = render_export_audio(&mut renderer, &project, 30, 2 * 30);

        // Output second 0 -> source second 0; output second 1 -> source second 3.
        assert!((left_at_second(&stream, 0) - expected(values[0])).abs() < 0.01);
        assert!((left_at_second(&stream, 1) - expected(values[3])).abs() < 0.01);
    }
}
