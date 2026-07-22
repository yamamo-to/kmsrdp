//! A self-driving [`DriveConsumer`] that proves the RDPDR wire protocol
//! round-trips against a real client, without requiring a full consumer
//! (e.g. a FUSE mount for drives, a CUPS backend for printers) to exist
//! yet - the same role `rdpcore_dvc::echo::EchoHandler` plays for the DVC
//! transport. For a filesystem device, this opens its root directory,
//! walks the listing logging every entry, reads a sample of the first
//! regular file it finds, and separately creates+writes+closes a small
//! marker file to exercise WRITE too. For a printer device, it opens a
//! synthetic print job, writes a few bytes to it, and closes it - proving
//! the identical CREATE/WRITE/CLOSE mechanism also round-trips for
//! RDPDR_DTYP_PRINT. Every step is reported through `on_event`.

use crate::{DriveCommand, DriveConsumer, irp, pdu};

const LIST_ALL_PATTERN: &str = "\\*";
const SAMPLE_READ_LENGTH: u32 = 4096;
const WRITE_TEST_FILE_NAME: &str = "\\kmsrdp_rdpdr_selftest.tmp";
const WRITE_TEST_CONTENTS: &[u8] = b"kmsrdp RDPDR write self-test\n";
const PRINT_JOB_TEST_CONTENTS: &[u8] = b"kmsrdp RDPDR print job self-test\n";

pub struct DirectoryListingSelfTest {
    on_event: Box<dyn FnMut(String) + Send>,
    device_id: u32,
    root_file_id: Option<u32>,
    sample_file_id: Option<u32>,
    sampled_a_file: bool,
    write_test_file_id: Option<u32>,
    print_job_file_id: Option<u32>,
}

impl DirectoryListingSelfTest {
    pub fn new(on_event: impl FnMut(String) + Send + 'static) -> Self {
        Self {
            on_event: Box::new(on_event),
            device_id: 0,
            root_file_id: None,
            sample_file_id: None,
            sampled_a_file: false,
            write_test_file_id: None,
            print_job_file_id: None,
        }
    }

    fn log(&mut self, message: impl Into<String>) {
        (self.on_event)(message.into());
    }
}

impl DriveConsumer for DirectoryListingSelfTest {
    fn on_device_ready(
        &mut self,
        device_id: u32,
        device_type: u32,
        dos_name: &str,
    ) -> Vec<DriveCommand> {
        self.device_id = device_id;

        if device_type == pdu::RDPDR_DTYP_PRINT {
            self.log(format!(
                "printer {device_id} ({dos_name}) ready - opening a test print job"
            ));
            return vec![DriveCommand::Create {
                device_id,
                path: String::new(), // print jobs aren't addressed by path
                desired_access: irp::GENERIC_WRITE | irp::SYNCHRONIZE,
                create_disposition: irp::FILE_OVERWRITE_IF,
                create_options: irp::FILE_SYNCHRONOUS_IO_NONALERT,
                request_tag: 10, // 10 = print job create reply
            }];
        }

        self.log(format!(
            "device {device_id} ({dos_name}) ready - opening root directory"
        ));
        vec![
            DriveCommand::Create {
                device_id,
                path: "\\".to_owned(),
                desired_access: irp::GENERIC_READ | irp::SYNCHRONIZE,
                create_disposition: irp::FILE_OPEN,
                create_options: irp::FILE_DIRECTORY_FILE | irp::FILE_SYNCHRONOUS_IO_NONALERT,
                request_tag: 1, // 1 = opening the root directory
            },
            DriveCommand::Create {
                device_id,
                path: WRITE_TEST_FILE_NAME.to_owned(),
                desired_access: irp::GENERIC_WRITE | irp::SYNCHRONIZE,
                create_disposition: irp::FILE_OVERWRITE_IF,
                create_options: irp::FILE_SYNCHRONOUS_IO_NONALERT,
                request_tag: 7, // 7 = write-test file create reply
            },
        ]
    }

    fn on_create_reply(
        &mut self,
        request_tag: u64,
        result: Result<irp::CreateReply, u32>,
    ) -> Vec<DriveCommand> {
        match (request_tag, result) {
            (1, Ok(reply)) => {
                self.root_file_id = Some(reply.file_id);
                self.log("root directory opened - listing entries");
                vec![DriveCommand::QueryDirectory {
                    device_id: self.device_id,
                    file_id: reply.file_id,
                    path: Some(LIST_ALL_PATTERN.to_owned()),
                    request_tag: 2, // 2 = directory-enumeration replies
                }]
            }
            (1, Err(status)) => {
                self.log(format!(
                    "failed to open root directory: NTSTATUS {status:#010x}"
                ));
                Vec::new()
            }
            (3, Ok(reply)) => {
                self.sample_file_id = Some(reply.file_id);
                self.log(format!(
                    "sample file opened (FileId {}) - reading first {SAMPLE_READ_LENGTH} bytes",
                    reply.file_id
                ));
                vec![DriveCommand::Read {
                    device_id: self.device_id,
                    file_id: reply.file_id,
                    length: SAMPLE_READ_LENGTH,
                    offset: 0,
                    request_tag: 4, // 4 = sample file read reply
                }]
            }
            (3, Err(status)) => {
                self.log(format!(
                    "failed to open sample file: NTSTATUS {status:#010x}"
                ));
                Vec::new()
            }
            (7, Ok(reply)) => {
                self.write_test_file_id = Some(reply.file_id);
                self.log(format!(
                    "write-test file created (FileId {}) - writing {} bytes",
                    reply.file_id,
                    WRITE_TEST_CONTENTS.len()
                ));
                vec![DriveCommand::Write {
                    device_id: self.device_id,
                    file_id: reply.file_id,
                    offset: 0,
                    data: WRITE_TEST_CONTENTS.to_vec(),
                    request_tag: 8, // 8 = write-test write reply
                }]
            }
            (7, Err(status)) => {
                self.log(format!(
                    "failed to create write-test file: NTSTATUS {status:#010x}"
                ));
                Vec::new()
            }
            (10, Ok(reply)) => {
                self.print_job_file_id = Some(reply.file_id);
                self.log(format!(
                    "print job opened (FileId {}) - writing {} bytes",
                    reply.file_id,
                    PRINT_JOB_TEST_CONTENTS.len()
                ));
                vec![DriveCommand::Write {
                    device_id: self.device_id,
                    file_id: reply.file_id,
                    offset: 0,
                    data: PRINT_JOB_TEST_CONTENTS.to_vec(),
                    request_tag: 11, // 11 = print job write reply
                }]
            }
            (10, Err(status)) => {
                self.log(format!("failed to open print job: NTSTATUS {status:#010x}"));
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn on_query_directory_reply(
        &mut self,
        request_tag: u64,
        result: Result<Option<irp::DirectoryEntry>, u32>,
    ) -> Vec<DriveCommand> {
        if request_tag != 2 {
            return Vec::new();
        }
        match result {
            Ok(Some(entry)) => {
                let is_dir = entry.file_attributes & irp::FILE_ATTRIBUTE_DIRECTORY != 0;
                self.log(format!(
                    "entry: {} ({}, {} bytes)",
                    entry.file_name,
                    if is_dir { "dir" } else { "file" },
                    entry.end_of_file
                ));

                let should_sample = !is_dir
                    && !self.sampled_a_file
                    && entry.file_name != "."
                    && entry.file_name != "..";
                if should_sample {
                    self.sampled_a_file = true;
                    return vec![DriveCommand::Create {
                        device_id: self.device_id,
                        path: format!("\\{}", entry.file_name),
                        desired_access: irp::GENERIC_READ | irp::SYNCHRONIZE,
                        create_disposition: irp::FILE_OPEN,
                        create_options: irp::FILE_SYNCHRONOUS_IO_NONALERT,
                        request_tag: 3, // 3 = sample file create reply
                    }];
                }

                let Some(root_file_id) = self.root_file_id else {
                    return Vec::new();
                };
                vec![DriveCommand::QueryDirectory {
                    device_id: self.device_id,
                    file_id: root_file_id,
                    path: None, // continue the enumeration
                    request_tag: 2,
                }]
            }
            Ok(None) => {
                self.log("directory listing complete");
                let Some(root_file_id) = self.root_file_id.take() else {
                    return Vec::new();
                };
                vec![DriveCommand::Close {
                    device_id: self.device_id,
                    file_id: root_file_id,
                    request_tag: 5,
                }]
            }
            Err(status) => {
                self.log(format!("directory query failed: NTSTATUS {status:#010x}"));
                Vec::new()
            }
        }
    }

    fn on_read_reply(
        &mut self,
        request_tag: u64,
        result: Result<Vec<u8>, u32>,
    ) -> Vec<DriveCommand> {
        if request_tag != 4 {
            return Vec::new();
        }
        match result {
            Ok(data) => {
                let preview = String::from_utf8_lossy(&data[..data.len().min(200)]);
                self.log(format!(
                    "read {} bytes from sample file - preview: {preview:?}",
                    data.len()
                ));
            }
            Err(status) => self.log(format!("sample file read failed: NTSTATUS {status:#010x}")),
        }
        let Some(file_id) = self.sample_file_id.take() else {
            return Vec::new();
        };
        vec![DriveCommand::Close {
            device_id: self.device_id,
            file_id,
            request_tag: 6,
        }]
    }

    fn on_write_reply(&mut self, request_tag: u64, result: Result<u32, u32>) -> Vec<DriveCommand> {
        match request_tag {
            8 => {
                match result {
                    Ok(bytes_written) => {
                        self.log(format!("wrote {bytes_written} bytes to write-test file"))
                    }
                    Err(status) => {
                        self.log(format!("write-test write failed: NTSTATUS {status:#010x}"))
                    }
                }
                let Some(file_id) = self.write_test_file_id.take() else {
                    return Vec::new();
                };
                vec![DriveCommand::Close {
                    device_id: self.device_id,
                    file_id,
                    request_tag: 9,
                }]
            }
            11 => {
                match result {
                    Ok(bytes_written) => {
                        self.log(format!("wrote {bytes_written} bytes to print job"))
                    }
                    Err(status) => {
                        self.log(format!("print job write failed: NTSTATUS {status:#010x}"))
                    }
                }
                let Some(file_id) = self.print_job_file_id.take() else {
                    return Vec::new();
                };
                vec![DriveCommand::Close {
                    device_id: self.device_id,
                    file_id,
                    request_tag: 12,
                }]
            }
            _ => Vec::new(),
        }
    }

    fn on_close_reply(&mut self, request_tag: u64, status: u32) -> Vec<DriveCommand> {
        match request_tag {
            5 => self.log(format!("root directory closed (status {status:#010x})")),
            6 => self.log(format!(
                "sample file closed (status {status:#010x}) - self-test complete"
            )),
            9 => self.log(format!("write-test file closed (status {status:#010x})")),
            12 => self.log(format!(
                "print job closed (status {status:#010x}) - print self-test complete"
            )),
            _ => {}
        }
        Vec::new()
    }

    fn on_set_information_reply(
        &mut self,
        _request_tag: u64,
        _result: Result<(), u32>,
    ) -> Vec<DriveCommand> {
        Vec::new()
    }
}
