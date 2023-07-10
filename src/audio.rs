use std::{
    cmp::max,
    ffi::{c_int, CString},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{channel, Receiver, RecvError, Sender, TryRecvError},
        Arc,
    },
    thread::spawn,
};

use anyhow::bail;
use ffmpeg::{
    codec::Context,
    decoder, encoder,
    ffi::{av_find_input_format, av_get_default_channel_layout, AVChannelOrder},
    filter,
    format::{self, context::Input, Sample},
    frame, ChannelLayout, Dictionary, Format, Packet, Rational,
};

use crate::{fifo::AudioFifo, Args};

struct AudioState {
    enc_audio: encoder::Audio,
    ist_stream_idx: usize,
    audio_stream_time_base: Rational,
    dec_audio: decoder::Audio,
    frame_sender: Sender<Packet>,

    audio_filter: filter::Graph,

    ost_audio_idx: usize,
    ost_audio_time_base: Rational,

    flush_flag: Arc<AtomicBool>,
    fifo: Option<AudioFifo>,

    pts: i64,
}

pub struct AudioHandle {
    rec: Receiver<Packet>,
    flush_flag: Arc<AtomicBool>,
}

pub struct IncompleteAudioState {
    input: format::context::Input,
    ist_stream_idx: usize,
    dec_audio: decoder::Audio,

    enc_audio: encoder::Audio,
    ost_stream_idx: usize,
}

impl AudioState {
    fn thread(mut self, mut audio_input: Input) {
        assert_ne!(self.ost_audio_time_base, Rational::new(0, 0));

        for (stream, mut packet) in audio_input.packets() {
            if stream.index() == self.ist_stream_idx {
                packet.rescale_ts(self.audio_stream_time_base, self.dec_audio.time_base());
                self.dec_audio.send_packet(&packet).unwrap();
                self.pop_from_decoder();
                self.pop_from_filter();
            }

            if self.flush_flag.load(Ordering::SeqCst) {
                self.flush();
                return;
            }
        }
    }

    fn pop_from_filter(&mut self) {
        let mut filtered_frame = frame::Audio::empty();
        while self
            .audio_filter
            .get("out")
            .unwrap()
            .sink()
            .frame(&mut filtered_frame)
            .is_ok()
        {
            if self.fifo.is_some() {
                self.fifo().unwrap().push(&filtered_frame);
                while self.fifo().unwrap().size() > self.enc_audio.frame_size() as usize {
                    let mut frame_into_encoder = frame::Audio::new(
                        self.enc_audio.format(),
                        self.enc_audio.frame_size() as usize,
                        self.enc_audio.channel_layout(),
                    );
                    self.fifo().unwrap().pop(&mut frame_into_encoder);
                    frame_into_encoder.set_rate(self.enc_audio.rate());
                    frame_into_encoder.set_pts(Some(self.pts));
                    self.pts += frame_into_encoder.samples() as i64;
                    self.enc_audio.send_frame(&frame_into_encoder).unwrap();
                    self.pop_frames_from_encoder();
                }
            } else {
                self.enc_audio.send_frame(&filtered_frame).unwrap();
                self.pop_frames_from_encoder();
            }
        }
    }

    fn fifo(&mut self) -> Option<&mut AudioFifo> {
        self.fifo.as_mut()
    }

    fn pop_from_decoder(&mut self) {
        let mut frame = frame::Audio::empty();
        while self.dec_audio.receive_frame(&mut frame).is_ok() {
            self.audio_filter
                .get("in")
                .unwrap()
                .source()
                .add(&frame)
                .unwrap();
        }
    }

    fn flush(&mut self) {
        self.dec_audio.send_eof().unwrap();
        self.pop_from_decoder();
        self.audio_filter
            .get("in")
            .unwrap()
            .source()
            .flush()
            .unwrap();
        self.pop_from_filter();
        self.enc_audio.send_eof().unwrap();
        self.pop_frames_from_encoder();
    }

    fn pop_frames_from_encoder(&mut self) {
        let mut pack = Packet::empty();
        while self.enc_audio.receive_packet(&mut pack).is_ok() {
            pack.set_stream(self.ost_audio_idx);
            pack.rescale_ts(self.dec_audio.time_base(), self.ost_audio_time_base);
            self.frame_sender
                .send(pack)
                .expect("Strange, main thread exited before issuing flush");

            pack = Packet::empty();
        }
    }
}

impl AudioHandle {
    pub fn create_stream(
        args: &Args,
        octx: &mut format::context::Output,
    ) -> anyhow::Result<IncompleteAudioState> {
        let codec = ffmpeg::encoder::find(
            octx.format()
                .codec(&args.output, ffmpeg::media::Type::Audio),
        )
        .unwrap()
        .audio()
        .unwrap();

        let mut ost_audio = octx.add_stream(codec).unwrap();

        let input_format = unsafe {
            let audio_backend = CString::new(args.audio_backend.clone()).unwrap();
            let fmt = av_find_input_format(audio_backend.as_ptr());
            if fmt.is_null() {
                bail!("Failed to acquire input format {}", args.audio_backend);
            }
            format::Input::wrap(fmt as _)
        };

        let audio_input = format::open_with(
            &args.audio_device,
            &Format::Input(input_format),
            Dictionary::default(),
        )
        .unwrap()
        .input();

        let best_audio_stream = audio_input
            .streams()
            .best(ffmpeg::media::Type::Audio)
            .unwrap();

        let dec_audio = Context::from_parameters(best_audio_stream.parameters())
            .unwrap()
            .decoder()
            .audio()
            .unwrap();

        let enc_audio_channel_layout = codec
            .channel_layouts()
            .map(|cls| cls.best(dec_audio.channel_layout().channels()))
            .unwrap_or(ChannelLayout::STEREO);

        let mut enc_audio = Context::from_parameters(ost_audio.parameters())
            .unwrap()
            .encoder()
            .audio()
            .unwrap();

        let audio_decoder_rate = dec_audio.rate() as i32;
        enc_audio.set_rate(audio_decoder_rate);
        enc_audio.set_channel_layout(enc_audio_channel_layout);
        enc_audio.set_channels(enc_audio_channel_layout.channels());
        let audio_encode_format = codec.formats().unwrap().next().unwrap();
        enc_audio.set_format(audio_encode_format);
        enc_audio.set_time_base(dec_audio.time_base());

        let enc_audio = enc_audio.open_as(codec).unwrap();

        ost_audio.set_parameters(&enc_audio);

        Ok(IncompleteAudioState {
            ist_stream_idx: best_audio_stream.index(),
            ost_stream_idx: ost_audio.index(),
            enc_audio,
            dec_audio,
            input: audio_input,
        })
    }

    pub fn try_recv(&mut self) -> Result<Packet, TryRecvError> {
        self.rec.try_recv()
    }

    pub fn recv(&mut self) -> Result<Packet, RecvError> {
        self.rec.recv()
    }

    pub fn start_flush(&mut self) {
        self.flush_flag.store(true, Ordering::SeqCst);
    }
}

impl IncompleteAudioState {
    pub fn finish(self, _args: &Args, octx: &format::context::Output) -> AudioHandle {
        let audio_stream_time_base = octx.stream(self.ost_stream_idx).unwrap().time_base();

        let mut fifo = None;
        if let Some(codec) = self.enc_audio.codec() {
            if !codec
                .capabilities()
                .contains(ffmpeg::codec::capabilities::Capabilities::VARIABLE_FRAME_SIZE)
            {
                fifo = Some(
                    AudioFifo::new(
                        self.enc_audio.format(),
                        self.enc_audio.channel_layout().channels(),
                        max(self.enc_audio.frame_size(), self.dec_audio.frame_size()) * 2,
                    )
                    .unwrap(),
                );
            }
        }

        let (frame_sender, r) = channel();

        let audio_filter = audio_filter(
            &self.dec_audio,
            self.dec_audio.rate() as i32,
            self.enc_audio.format(),
            self.enc_audio.channel_layout(),
        );

        let flush_flag = Arc::new(AtomicBool::new(false));

        let audiostate = AudioState {
            // fifo: None,
            enc_audio: self.enc_audio,
            // audio_input,
            ist_stream_idx: self.ist_stream_idx,
            audio_stream_time_base,
            dec_audio: self.dec_audio,
            frame_sender,
            ost_audio_idx: self.ost_stream_idx,
            ost_audio_time_base: audio_stream_time_base,
            audio_filter,
            flush_flag: flush_flag.clone(),
            fifo,
            pts: 0,
        };
        spawn(move || audiostate.thread(self.input));

        AudioHandle { rec: r, flush_flag }
    }
}

fn audio_filter(
    // input: &ffmpeg::Stream,
    input: &decoder::Audio,
    codec_sample_rate: i32,
    codec_sample_format: Sample,
    codec_channel_layout: ChannelLayout,
) -> filter::Graph {
    let mut g = ffmpeg::filter::graph::Graph::new();

    // let channel_format_str = avchannelformat_to_string(params.ch_layout);
    let sample_format = input.format();

    let ch_layout = unsafe { input.as_ptr().read().ch_layout };
    let ch_layout_mask = if ch_layout.order == AVChannelOrder::AV_CHANNEL_ORDER_NATIVE {
        unsafe { ch_layout.u.mask }
    } else {
        unsafe { av_get_default_channel_layout(input.channels() as c_int) as u64 }
    };

    g.add(
        &filter::find("abuffer").unwrap(),
        "in",
        &format!(
            "sample_rate={}:sample_fmt={}:channel_layout={:#x}",
            input.rate(),
            sample_format.name(),
            ch_layout_mask
        ),
    )
    .unwrap();

    g.add(&filter::find("abuffersink").unwrap(), "out", "")
        .unwrap();

    g.output("in", 0)
        .unwrap()
        .input("out", 0)
        .unwrap()
        .parse(&format!(
            "aformat=sample_rates={}:sample_fmts={}:channel_layouts={:#x}",
            codec_sample_rate,
            codec_sample_format.name(),
            codec_channel_layout.bits(),
            // avchannelformat_to_string(avchannelformat_from_bits(codec_channel_layout.bits())),
        ))
        .unwrap();
    g.validate().unwrap();

    g
}