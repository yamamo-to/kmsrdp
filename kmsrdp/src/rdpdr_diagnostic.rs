//! Validation-only RDPDR consumer: proves the from-scratch drive
//! redirection protocol round-trips against a real client by
//! automatically walking whatever filesystem device the client announces
//! (e.g. via `xfreerdp /drive:share,/path`) and logging what it finds.
//!
//! Not a real feature yet - browsing the redirected drive from within the
//! remote Linux desktop session (e.g. via a FUSE mount) is a follow-up;
//! this only proves the wire protocol itself works.

use rdpcore_rdpdr::diagnostic::DirectoryListingSelfTest;
use rdpcore_rdpdr::pdu::{RDPDR_DTYP_FILESYSTEM, RDPDR_DTYP_PRINT};
use rdpcore_rdpdr::{DriveConsumer, DriveConsumerFactory};

pub struct DiagnosticDriveFactory;

impl DriveConsumerFactory for DiagnosticDriveFactory {
    fn supported_device_types(&self) -> u32 {
        RDPDR_DTYP_FILESYSTEM | RDPDR_DTYP_PRINT
    }

    fn build_drive_consumer(&self) -> Box<dyn DriveConsumer> {
        Box::new(DirectoryListingSelfTest::new(|event| {
            println!("rdpdr self-test: {event}")
        }))
    }
}
