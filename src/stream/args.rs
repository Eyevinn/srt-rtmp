use clap::{Parser, ValueEnum};

#[derive(Parser, Debug, Clone)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    /// SRT source stream address(ip:port)
    #[clap(short, long)]
    pub input_address: String,

    /// SRT mode to use:
    /// 1) caller - run a discoverer and then connect to the SRT stream (in listener mode).
    /// 2) listener - wait for a SRT stream (in caller mode) to connect.
    #[clap(short, long, value_enum, verbatim_doc_comment)]
    pub srt_mode: SRTMode,

    /// RTMP output stream address(ip:port/application/)
    #[clap(short, long)]
    pub output_address: String,

    /// RTMP output stream key(confidential)
    #[clap(short, long)]
    pub key: Option<String>,
}

#[derive(ValueEnum, Clone, Debug, PartialEq)]
pub enum SRTMode {
    Caller,
    Listener,
}

impl SRTMode {
    pub fn to_str(&self) -> &str {
        match self {
            SRTMode::Caller => "caller",
            SRTMode::Listener => "listener",
        }
    }

    pub fn reverse(&self) -> Self {
        match self {
            SRTMode::Caller => SRTMode::Listener,
            SRTMode::Listener => SRTMode::Caller,
        }
    }
}
