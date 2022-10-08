use crate::{version, Message};

#[derive(Debug)]
pub(crate) struct Envelope {
    /// Who sent the message
    pub(crate) src: version::Dot,

    /// Message value
    pub(crate) message: Box<dyn Message>,
}
