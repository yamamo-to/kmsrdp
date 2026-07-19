//! Optional RDPDR consumer (feature `rdpdr-diagnostic`): proves the
//! drive-redirection protocol round-trips against a real client by walking
//! whatever filesystem device the client announces and logging results.
//!
//! Production builds use [`crate::rdpdr_fuse`] instead. Enable with
//! `--features rdpdr-diagnostic` and set `KMSRDP_RDPDR_DIAGNOSTIC=1`.

use rdpcore_rdpdr::diagnostic::DirectoryListingSelfTest;
use rdpcore_rdpdr::pdu::{RDPDR_DTYP_FILESYSTEM, RDPDR_DTYP_PRINT};
use rdpcore_rdpdr::{DriveConsumer, DriveConsumerFactory};
use tokio::sync::mpsc::UnboundedSender;

pub struct DiagnosticDriveFactory;

impl DriveConsumerFactory for DiagnosticDriveFactory {
    fn supported_device_types(&self) -> u32 {
        RDPDR_DTYP_FILESYSTEM | RDPDR_DTYP_PRINT
    }

    fn build_drive_consumer(&self, _wake: UnboundedSender<()>) -> Box<dyn DriveConsumer> {
        Box::new(DirectoryListingSelfTest::new(|event| {
            println!("rdpdr self-test: {event}")
        }))
    }
}
