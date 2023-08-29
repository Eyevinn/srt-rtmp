use clap::Parser;
use srt_rtmp::stream::{Args, SharablePipeline};
use srt_rtmp::telemetry::{get_subscriber, init_subscriber};
use srt_rtmp::utils::PipelineGuard;
use std::error::Error;
use std::time::Duration;
use tokio::time::sleep;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();

    let subscriber = get_subscriber("srt_rtmp".into(), "debug".into(), std::io::stdout);
    init_subscriber(subscriber);

    let pipeline = SharablePipeline::new();
    loop {
        let mut pipeline_guard = PipelineGuard::new(pipeline.clone(), args.clone());

        if let Err(err) = pipeline_guard.run().await {
            tracing::error!("Pipeline runs into error: {}", err);
        } else {
            tracing::info!("Pipeline reaches EOS. Reset and rerun the pipeline.");
        }

        sleep(Duration::from_secs(5)).await;
    }
}
