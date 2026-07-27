//! Deliberately hostile `IStream` implementations.
//!
//! `IInitializeWithStream::Initialize` is the provider's entire input surface,
//! and in production the stream on the other end is supplied by the shell. The
//! provider's read loop already contains defensive code - a `Stat`-based fast
//! reject, a running size cap, and a fallback for when `Stat` fails - but none
//! of that is reachable from a well-behaved `SHCreateMemStream`, which always
//! reports the truth and always fills the buffer.
//!
//! These streams break each of those assumptions individually so the defensive
//! branches actually execute. The contract under test is not "produce a correct
//! thumbnail" but "never crash, never hang, never read out of bounds, and never
//! commit unbounded memory, no matter what the stream claims".

use std::ffi::c_void;
use std::sync::Mutex;

use windows::core::*;
use windows::Win32::Foundation::{E_FAIL, S_FALSE, S_OK};
use windows::Win32::System::Com::{
    IStream, IStream_Impl, ISequentialStream_Impl, LOCKTYPE, STATFLAG, STATSTG, STGC, STREAM_SEEK,
};

/// What the stream reports when the provider calls `Stat` to size the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatMode {
    /// Report the true length.
    Truthful,
    /// Fail the call, forcing the provider onto its "size unknown" path where
    /// the only protection left is the running cap inside the read loop.
    Fail,
    /// Claim u64::MAX bytes. A provider that pre-allocated from this number
    /// would abort the process on a failed allocation.
    Huge,
    /// Claim zero while actually holding data.
    Zero,
    /// Claim more than the provider's documented 101 MiB ceiling so the fast
    /// reject fires without any data being transferred.
    OverCap,
    /// Claim far less than the stream really holds, so the fast reject passes
    /// and only the in-loop cap can stop an oversized file.
    UnderReport,
}

/// How the stream behaves once the provider starts reading.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadMode {
    /// Fill every request completely.
    Truthful,
    /// Hand back a single byte per call - the pathological but legal case that
    /// proves the loop reassembles chunk boundaries correctly.
    OneByte,
    /// Never deliver more than `n` bytes per call.
    Short(u32),
    /// Deliver `n` bytes successfully, then fail. The provider ends up with a
    /// truncated document and must degrade rather than crash.
    FailAfter(usize),
    /// Immediately report success with zero bytes read, forever. A loop that
    /// only terminated on error would spin here.
    ZeroForever,
    /// Report having written MORE bytes than were requested. This is a protocol
    /// violation that a caller could turn into an out-of-bounds read if it
    /// trusted the count; the provider must not.
    OverReport,
    /// Succeed without ever updating the caller's byte count.
    NeverSetCount,
    /// Return S_FALSE with a partial read, which is legal for
    /// `ISequentialStream::Read` and must be treated as success.
    SFalse,
}

struct State {
    data: Vec<u8>,
    pos: usize,
    delivered: usize,
    read_calls: u32,
}

/// An `IStream` whose `Stat` and `Read` behaviour is configurable per test case.
#[implement(IStream)]
pub struct HostileStream {
    state: Mutex<State>,
    stat_mode: StatMode,
    read_mode: ReadMode,
    /// Value reported by `Stat` when `stat_mode` is `UnderReport`.
    under_report_to: u64,
}

impl HostileStream {
    pub fn new(data: Vec<u8>, stat_mode: StatMode, read_mode: ReadMode) -> IStream {
        let under_report_to = if data.is_empty() { 0 } else { 1 };
        let this = HostileStream {
            state: Mutex::new(State { data, pos: 0, delivered: 0, read_calls: 0 }),
            stat_mode,
            read_mode,
            under_report_to,
        };
        this.into()
    }

    fn reported_size(&self, actual: u64) -> u64 {
        match self.stat_mode {
            StatMode::Truthful | StatMode::Fail => actual,
            StatMode::Huge => u64::MAX,
            StatMode::Zero => 0,
            // Comfortably past the provider's 101 MiB limit.
            StatMode::OverCap => 200 * 1024 * 1024,
            StatMode::UnderReport => self.under_report_to,
        }
    }
}

impl ISequentialStream_Impl for HostileStream_Impl {
    fn Read(&self, pv: *mut c_void, cb: u32, pcbread: *mut u32) -> HRESULT {
        // A null destination with a non-zero count is itself a protocol error;
        // refuse rather than write through it.
        if pv.is_null() && cb != 0 {
            return E_FAIL;
        }
        let mut state = match self.state.lock() {
            Ok(s) => s,
            Err(_) => return E_FAIL,
        };
        state.read_calls += 1;

        // Decide how many bytes this call is allowed to hand back.
        let remaining = state.data.len().saturating_sub(state.pos);
        let allowed = match self.read_mode {
            ReadMode::Truthful | ReadMode::SFalse | ReadMode::OverReport
            | ReadMode::NeverSetCount => remaining.min(cb as usize),
            ReadMode::OneByte => remaining.min(cb as usize).min(1),
            ReadMode::Short(n) => remaining.min(cb as usize).min(n as usize),
            ReadMode::ZeroForever => 0,
            ReadMode::FailAfter(limit) => {
                if state.delivered >= limit {
                    // Report the failure without touching pcbread, exactly as a
                    // failing filesystem stream would.
                    return E_FAIL;
                }
                remaining.min(cb as usize).min(limit - state.delivered)
            }
        };

        if allowed > 0 {
            unsafe {
                std::ptr::copy_nonoverlapping(
                    state.data.as_ptr().add(state.pos),
                    pv as *mut u8,
                    allowed,
                );
            }
            state.pos += allowed;
            state.delivered += allowed;
        }

        if !pcbread.is_null() {
            let reported = match self.read_mode {
                // The hostile case: claim to have filled far more of the
                // caller's buffer than was requested.
                ReadMode::OverReport => cb.saturating_mul(4).max(cb + 4096),
                ReadMode::NeverSetCount => {
                    // Leave the caller's variable untouched.
                    return if allowed > 0 { S_OK } else { S_OK };
                }
                _ => allowed as u32,
            };
            unsafe { *pcbread = reported };
        }

        match self.read_mode {
            ReadMode::SFalse if allowed < cb as usize => S_FALSE,
            _ => S_OK,
        }
    }

    fn Write(&self, _pv: *const c_void, _cb: u32, _pcbwritten: *mut u32) -> HRESULT {
        // The provider only ever reads. A write attempt would be a bug in the
        // test, so make it loudly unsupported.
        windows::Win32::Foundation::E_NOTIMPL
    }
}

impl IStream_Impl for HostileStream_Impl {
    fn Seek(&self, dlibmove: i64, dworigin: STREAM_SEEK, plibnewposition: *mut u64) -> Result<()> {
        let mut state = self.state.lock().map_err(|_| Error::from(E_FAIL))?;
        let len = state.data.len() as i64;
        let base = match dworigin {
            windows::Win32::System::Com::STREAM_SEEK_SET => 0,
            windows::Win32::System::Com::STREAM_SEEK_CUR => state.pos as i64,
            windows::Win32::System::Com::STREAM_SEEK_END => len,
            _ => return Err(Error::from(E_FAIL)),
        };
        let target = base.saturating_add(dlibmove).clamp(0, len);
        state.pos = target as usize;
        if !plibnewposition.is_null() {
            unsafe { *plibnewposition = state.pos as u64 };
        }
        Ok(())
    }

    fn Stat(&self, pstatstg: *mut STATSTG, _grfstatflag: &STATFLAG) -> Result<()> {
        if self.stat_mode == StatMode::Fail {
            return Err(Error::new(E_FAIL, "Stat deliberately failed"));
        }
        if pstatstg.is_null() {
            return Err(Error::from(windows::Win32::Foundation::E_POINTER));
        }
        let state = self.state.lock().map_err(|_| Error::from(E_FAIL))?;
        let actual = state.data.len() as u64;
        unsafe {
            let stat = &mut *pstatstg;
            *stat = STATSTG::default();
            stat.r#type = 2; // STGTY_STREAM
            stat.cbSize = self.reported_size(actual);
        }
        Ok(())
    }

    fn SetSize(&self, _libnewsize: u64) -> Result<()> {
        Err(Error::from(windows::Win32::Foundation::E_NOTIMPL))
    }

    fn CopyTo(
        &self,
        _pstm: Ref<'_, IStream>,
        _cb: u64,
        _pcbread: *mut u64,
        _pcbwritten: *mut u64,
    ) -> Result<()> {
        Err(Error::from(windows::Win32::Foundation::E_NOTIMPL))
    }

    fn Commit(&self, _grfcommitflags: &STGC) -> Result<()> {
        Err(Error::from(windows::Win32::Foundation::E_NOTIMPL))
    }

    fn Revert(&self) -> Result<()> {
        Err(Error::from(windows::Win32::Foundation::E_NOTIMPL))
    }

    fn LockRegion(&self, _liboffset: u64, _cb: u64, _dwlocktype: &LOCKTYPE) -> Result<()> {
        Err(Error::from(windows::Win32::Foundation::E_NOTIMPL))
    }

    fn UnlockRegion(&self, _liboffset: u64, _cb: u64, _dwlocktype: u32) -> Result<()> {
        Err(Error::from(windows::Win32::Foundation::E_NOTIMPL))
    }

    fn Clone(&self) -> Result<IStream> {
        // The provider never clones its input stream; if that ever changes this
        // becoming reachable is itself worth noticing.
        Err(Error::from(windows::Win32::Foundation::E_NOTIMPL))
    }
}
