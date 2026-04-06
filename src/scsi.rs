// freemkv-info — BD drive probe
// AGPL-3.0 — freemkv project
//
// Low-level SCSI SG_IO interface

use std::fs::OpenOptions;
use std::io;
use std::os::unix::io::AsRawFd;

const SG_IO: u32 = 0x2285;
const SG_DXFER_FROM_DEV: i32 = -3;

#[repr(C)]
#[allow(non_camel_case_types)]
struct sg_io_hdr {
    interface_id: i32,
    dxfer_direction: i32,
    cmd_len: u8,
    mx_sb_len: u8,
    iovec_count: u16,
    dxfer_len: u32,
    dxferp: *mut u8,
    cmdp: *const u8,
    sbp: *mut u8,
    timeout: u32,
    flags: u32,
    pack_id: i32,
    usr_ptr: *mut u8,
    status: u8,
    masked_status: u8,
    msg_status: u8,
    sb_len_wr: u8,
    host_status: u16,
    driver_status: u16,
    resid: i32,
    duration: u32,
    info: u32,
}

pub struct ScsiDevice {
    fd: i32,
    _file: std::fs::File,
}

impl ScsiDevice {
    pub fn open(path: &str) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        Ok(ScsiDevice {
            fd: file.as_raw_fd(),
            _file: file,
        })
    }

    /// Send a SCSI command and return the response data.
    /// Returns None if the command failed (CHECK CONDITION or error).
    pub fn command(&self, cdb: &[u8], response_len: usize) -> Option<Vec<u8>> {
        let mut response = vec![0u8; response_len];
        let mut sense = [0u8; 64];

        let hdr = sg_io_hdr {
            interface_id: b'S' as i32,
            dxfer_direction: SG_DXFER_FROM_DEV,
            cmd_len: cdb.len() as u8,
            mx_sb_len: 64,
            iovec_count: 0,
            dxfer_len: response_len as u32,
            dxferp: response.as_mut_ptr(),
            cmdp: cdb.as_ptr(),
            sbp: sense.as_mut_ptr(),
            timeout: 10000, // 10 seconds
            flags: 0,
            pack_id: 0,
            usr_ptr: std::ptr::null_mut(),
            status: 0,
            masked_status: 0,
            msg_status: 0,
            sb_len_wr: 0,
            host_status: 0,
            driver_status: 0,
            resid: 0,
            duration: 0,
            info: 0,
        };

        let ret = unsafe {
            libc::ioctl(self.fd, SG_IO as _, &hdr as *const sg_io_hdr)
        };

        if ret < 0 || hdr.status != 0 {
            return None;
        }

        // Trim to actual bytes transferred
        let actual = response_len - hdr.resid.max(0) as usize;
        response.truncate(actual);
        Some(response)
    }
}
