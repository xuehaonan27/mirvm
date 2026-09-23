//! The record the whole format exists for: one intercepted syscall, as its entry and its exit.
//!
//! The entry carries the number and the six arguments, the exit carries the result and the errno
//! that path chose to expose, and [`SyscallSemantics`] is the field that tells a reader which
//! interpretation the rest of the record follows.

use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SyscallSemantics {
    Raw,
    Libc,
}

impl SyscallSemantics {
    const fn flag(self) -> u8 {
        match self {
            Self::Raw => FLAG_RAW,
            Self::Libc => FLAG_LIBC,
        }
    }

    fn from_control(control: Control, expected_kind: u16) -> Result<Self, WireError> {
        if control.kind != expected_kind
            || control.version != 0
            || control.flags & FLAG_CONTEXT_CONTROL != 0
        {
            return Err(WireError::new(
                "invalid syscall record kind or context flag",
            ));
        }
        match control.flags & (FLAG_RAW | FLAG_LIBC) {
            FLAG_RAW => Ok(Self::Raw),
            FLAG_LIBC => Ok(Self::Libc),
            _ => Err(WireError::new(
                "syscall record must set exactly one of RAW and LIBC",
            )),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SyscallEnter {
    pub(crate) semantics: SyscallSemantics,
    pub(crate) nr: i64,
    pub(crate) args: [u64; 6],
}

impl SyscallEnter {
    pub(crate) fn to_le_bytes(&self) -> [u8; SYSCALL_ENTER_BYTES] {
        let mut out = [0_u8; SYSCALL_ENTER_BYTES];
        put(
            &mut out,
            0,
            &Control::new(
                KIND_SYSCALL_ENTER,
                self.semantics.flag(),
                SYSCALL_ENTER_BYTES,
            )
            .to_le_bytes(),
        );
        put_i64(&mut out, 8, self.nr);
        for (index, arg) in self.args.iter().enumerate() {
            put_u64(&mut out, 16 + index * 8, *arg);
        }
        out
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        require_exact_len(bytes, SYSCALL_ENTER_BYTES, "SyscallEnter")?;
        let control = Control::decode(bytes)?;
        if control.byte_len() != SYSCALL_ENTER_BYTES {
            return Err(WireError::new("SyscallEnter has wrong record length"));
        }
        let semantics = SyscallSemantics::from_control(control, KIND_SYSCALL_ENTER)?;
        let mut args = [0_u64; 6];
        for (index, arg) in args.iter_mut().enumerate() {
            *arg = read_u64(bytes, 16 + index * 8, "syscall argument")?;
        }
        Ok(Self {
            semantics,
            nr: read_i64(bytes, 8, "syscall number")?,
            args,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SyscallExit {
    pub(crate) semantics: SyscallSemantics,
    pub(crate) result: i64,
    pub(crate) errno: Option<u32>,
}

impl SyscallExit {
    pub(crate) fn to_le_bytes(&self) -> Result<[u8; SYSCALL_EXIT_BYTES], WireError> {
        self.validate()?;
        let mut out = [0_u8; SYSCALL_EXIT_BYTES];
        put(
            &mut out,
            0,
            &Control::new(KIND_SYSCALL_EXIT, self.semantics.flag(), SYSCALL_EXIT_BYTES)
                .to_le_bytes(),
        );
        put_i64(&mut out, 8, self.result);
        let status = self
            .errno
            .map_or(0, |errno| u64::from(errno) | STATUS_ERRNO_VALID);
        put_u64(&mut out, 16, status);
        Ok(out)
    }

    fn validate(&self) -> Result<(), WireError> {
        match self.semantics {
            SyscallSemantics::Raw if self.errno.is_none() => Ok(()),
            SyscallSemantics::Raw => Err(WireError::new("raw SyscallExit cannot carry libc errno")),
            SyscallSemantics::Libc if self.result == -1 && self.errno.is_some() => Ok(()),
            SyscallSemantics::Libc if self.result == -1 => Err(WireError::new(
                "failed libc SyscallExit must carry a valid errno",
            )),
            SyscallSemantics::Libc if self.errno.is_none() => Ok(()),
            SyscallSemantics::Libc => Err(WireError::new(
                "successful libc SyscallExit must have zero status",
            )),
        }
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        require_exact_len(bytes, SYSCALL_EXIT_BYTES, "SyscallExit")?;
        let control = Control::decode(bytes)?;
        if control.byte_len() != SYSCALL_EXIT_BYTES {
            return Err(WireError::new("SyscallExit has wrong record length"));
        }
        let semantics = SyscallSemantics::from_control(control, KIND_SYSCALL_EXIT)?;
        let result = read_i64(bytes, 8, "syscall result")?;
        let status = read_u64(bytes, 16, "syscall status")?;
        if status & !STATUS_KNOWN_MASK != 0 {
            return Err(WireError::new(format!(
                "SyscallExit status has reserved bits: 0x{status:016x}"
            )));
        }
        let errno = (status & STATUS_ERRNO_VALID != 0).then_some(status as u32);
        if errno.is_none() && status & STATUS_ERRNO_MASK != 0 {
            return Err(WireError::new(
                "SyscallExit has errno bits without errno_valid",
            ));
        }
        let exit = Self {
            semantics,
            result,
            errno,
        };
        exit.validate()?;
        Ok(exit)
    }
}
