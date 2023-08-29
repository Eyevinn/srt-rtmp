use anyhow::{Error, Ok, Result};
use gst::{prelude::*, DebugGraphDetails, Pipeline};
use gstreamer as gst;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::time::Duration;
use timed_locks::Mutex;

use crate::stream::args::Args;
use crate::stream::MyError;

#[derive(Clone)]
pub struct PipelineWrapper {
    pipeline: Option<Pipeline>,
}

impl PipelineWrapper {
    pub fn new() -> Self {
        Self { pipeline: None }
    }
}

impl Default for PipelineWrapper {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone)]
pub struct SharablePipeline(Arc<Mutex<PipelineWrapper>>);

impl Deref for SharablePipeline {
    type Target = Arc<Mutex<PipelineWrapper>>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for SharablePipeline {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl SharablePipeline {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new_with_timeout(
            PipelineWrapper::new(),
            Duration::from_secs(1),
        )))
    }
}

impl Default for SharablePipeline {
    fn default() -> Self {
        Self::new()
    }
}

impl SharablePipeline {
    /// Check if SRT input stream is available
    pub async fn ready(&self) -> Result<bool, Error> {
        let pipeline_state = self.lock_err().await?;
        let pipeline = pipeline_state.pipeline.as_ref().unwrap();

        let demux = pipeline
            .by_name("demux")
            .ok_or(MyError::MissingElement("demux".to_string()))?;

        Ok(demux
            .pads()
            .into_iter()
            .any(|pad| pad.name().starts_with("video") || pad.name().starts_with("audio")))
    }

    /// Setup pipeline
    /// # Arguments
    /// * `args` - Pipeline arguments
    ///
    /// Create a pipeline with the all needed elements and register callbacks for dynamic pads
    /// Link them together when the demux element generates all dynamic pads and start playing
    pub async fn init(&mut self, args: &Args) -> Result<(), Error> {
        // Initialize GStreamer (only once)
        gst::init()?;
        tracing::debug!("Setting up pipeline");

        // Create a pipeline
        let pipeline = gst::Pipeline::default();

        let uri = format!(
            "srt://{}?mode={}",
            args.input_address,
            args.srt_mode.to_str()
        );
        tracing::info!("SRT  Input  uri: {}", uri);
        let stream_key =
            std::env::var("STREAM_KEY").or(args.key.clone().ok_or(MyError::MissingStreamKey));
        if stream_key.is_err() {
            tracing::error!("No stream key found from env or args");
            return Err(stream_key.unwrap_err().into());
        }

        tracing::info!("RTMP Output uri: rtmp://{}", args.output_address);
        let rtmp_address = format!("rtmp://{}/{}", args.output_address, stream_key.unwrap());

        let src = gst::ElementFactory::make("srtsrc")
            .property("uri", uri)
            .build()?;
        let demux_queue = gst::ElementFactory::make("queue")
            .name("demux_queue")
            .build()?;
        let typefind = gst::ElementFactory::make("typefind")
            .name("typefind")
            .build()?;
        let tsdemux = gst::ElementFactory::make("tsdemux").name("demux").build()?;

        let video_queue: gst::Element = gst::ElementFactory::make("queue")
            .name("video-queue")
            .build()?;
        let h264parse = gst::ElementFactory::make("h264parse").build()?;
        let audio_queue: gst::Element = gst::ElementFactory::make("queue")
            .name("audio-queue")
            .build()?;
        let aacparse = gst::ElementFactory::make("aacparse").build()?;
        let flvmux = gst::ElementFactory::make("flvmux").name("flvmux").build()?;

        let flv_queue = gst::ElementFactory::make("queue")
            .name("flv_queue")
            .build()?;
        let rtmpsink = gst::ElementFactory::make("rtmpsink")
            .name("rtmpsink")
            .property("location", &rtmp_address)
            .build()?;

        pipeline.add_many([
            &src,
            &demux_queue,
            &typefind,
            &tsdemux,
            &video_queue,
            &h264parse,
            &audio_queue,
            &aacparse,
            &flvmux,
            &flv_queue,
            &rtmpsink,
        ])?;
        gst::Element::link_many([&src, &demux_queue, &typefind, &tsdemux])?;
        gst::Element::link_many([&video_queue, &h264parse, &flvmux])?;
        gst::Element::link_many([&audio_queue, &aacparse, &flvmux])?;
        gst::Element::link_many([&flvmux, &flv_queue, &rtmpsink])?;

        let pipeline_weak = pipeline.downgrade();
        // Connect to tsdemux's no-more-pads signal, that is emitted when the element
        // will not generate more dynamic pads. This usually happens when the stream
        // is fully received and decoded.
        tsdemux.connect_no_more_pads(move |_dbin| {
            tracing::info!("No more pads from the stream. Ready to link.");
            // Here we temporarily retrieve a strong reference on the pipeline from the weak one
            // we moved into this callback.
            let pipeline = match pipeline_weak.upgrade() {
                Some(pipeline) => pipeline,
                None => return,
            };

            let rtmpsink = pipeline
                .by_name("rtmpsink")
                .ok_or(MyError::MissingElement("rtmpsink".to_string()));

            let success = rtmpsink.is_ok_and(|rtmpsink| rtmpsink.sync_state_with_parent().is_ok());

            if success {
                tracing::info!("Successfully started rmtp streaming");
                pipeline.debug_to_dot_file(DebugGraphDetails::all(), "pipeline");
            } else {
                tracing::error!("Failed to start rmtp streaming");
            }
        });

        let pipeline_weak = pipeline.downgrade();
        // Connect to decodebin's pad-added signal, that is emitted whenever
        // it found another stream from the input file and found a way to decode it to its raw format.
        // decodebin automatically adds a src-pad for this raw stream, which
        // we can use to build the follow-up pipeline.
        tsdemux.connect_pad_added(move |_dbin, src_pad| {
            // Here we temporarily retrieve a strong reference on the pipeline from the weak one
            // we moved into this callback.
            let pipeline = match pipeline_weak.upgrade() {
                Some(pipeline) => pipeline,
                None => return,
            };

            // Try to detect whether the raw stream decodebin provided us with
            // just now is either audio or video (or none of both, e.g. subtitles).
            let (is_audio, is_video) = {
                let media_type = src_pad.current_caps().and_then(|caps| {
                    caps.structure(0).map(|s| {
                        let name = s.name();
                        (name.starts_with("audio/"), name.starts_with("video/"))
                    })
                });

                match media_type {
                    None => {
                        tracing::error!("Unknown pad added {:?}", src_pad);
                        return;
                    }
                    Some(media_type) => media_type,
                }
            };

            let insert_sink = |is_audio, is_video| -> Result<(), Error> {
                if is_audio {
                    // Get the queue element's sink pad and link the decodebin's newly created
                    // src pad for the audio stream to it.
                    let audio_queue = pipeline
                        .by_name("audio-queue")
                        .ok_or(MyError::MissingElement("audio-queue".to_string()))?;
                    let sink_pad =
                        audio_queue
                            .static_pad("sink")
                            .ok_or(MyError::MissingElement(
                                "audio-queue's sink pad".to_string(),
                            ))?;
                    src_pad.link(&sink_pad)?;

                    tracing::info!("Successfully inserted audio sink");
                }
                if is_video {
                    // Get the queue element's sink pad and link the decodebin's newly created
                    // src pad for the video stream to it.
                    let video_queue = pipeline
                        .by_name("video-queue")
                        .ok_or(MyError::MissingElement("video-queue".to_string()))?;
                    let sink_pad =
                        video_queue
                            .static_pad("sink")
                            .ok_or(MyError::MissingElement(
                                "video-queue's sink pad".to_string(),
                            ))?;
                    src_pad.link(&sink_pad)?;

                    tracing::info!("Successfully inserted video sink");
                }

                Ok(())
            };

            if let Err(err) = insert_sink(is_audio, is_video) {
                tracing::error!("Failed to insert sink: {}", err);
            }
        });

        // Set to playing
        pipeline.set_state(gst::State::Playing)?;
        {
            self.lock_err().await?.pipeline = Some(pipeline);
        }

        Ok(())
    }

    /// Run pipeline and wait until the message bus receives an EOS or error message
    pub async fn run(&self) -> Result<(), Error> {
        let pipeline_state = self.lock_err().await?;
        let pipeline = pipeline_state
            .pipeline
            .as_ref()
            .ok_or(MyError::FailedOperation(
                "Pipeline called before initialization".to_string(),
            ))?;
        let bus = pipeline.bus().unwrap();
        drop(pipeline_state);

        let main_loop = glib::MainLoop::new(None, false);
        // Wait until an EOS or error message appears
        let main_loop_clone = main_loop.clone();
        let _bus_watch = bus.add_watch(move |_, msg| {
            use gst::MessageView;

            let main_loop = &main_loop_clone;
            match msg.view() {
                MessageView::Eos(..) => {
                    tracing::info!("received eos");
                    // An EndOfStream event was sent to the pipeline, so we tell our main loop
                    // to stop execution here.
                    main_loop.quit();
                }
                MessageView::Error(err) => {
                    tracing::error!(
                        "{:?} runs into error : {} ({:?})",
                        err.src().map(|s| s.path_string()),
                        err.error(),
                        err.debug()
                    );
                    main_loop.quit();
                }
                _ => (),
            };

            // Tell the mainloop to continue executing this callback.
            glib::ControlFlow::Continue
        })?;

        // Operate GStreamer's bus, facilliating GLib's mainloop here.
        // This function call will block until you tell the mainloop to quit
        main_loop.run();

        Ok(())
    }

    /// Close pipeline by sending EOS message
    pub async fn end(&self) -> Result<(), Error> {
        let pipeline_state = self.lock_err().await?;
        if let Some(pipeline) = pipeline_state.pipeline.as_ref() {
            pipeline.send_event(gst::event::Eos::new());
        }

        Ok(())
    }

    /// Clean up all elements in the pipeline and reset state
    pub async fn clean_up(&self) -> Result<(), Error> {
        let mut pipeline_state = self.lock_err().await?;
        if let Some(pipeline) = pipeline_state.pipeline.as_ref() {
            pipeline.call_async(move |pipeline| {
                let _ = pipeline.set_state(gst::State::Null);
            });
            pipeline_state.pipeline = None;
        }

        Ok(())
    }

    /// Helper function for debugging
    /// Print pipeline to dot file
    /// Warning: this function should not be called when the pipeline is in locked state
    pub async fn print(&self) -> Result<(), Error> {
        let pipeline_state = self.lock_err().await?;
        if let Some(pipeline) = pipeline_state.pipeline.as_ref() {
            pipeline.debug_to_dot_file(DebugGraphDetails::all(), "pipeline");
        }

        Ok(())
    }
}
