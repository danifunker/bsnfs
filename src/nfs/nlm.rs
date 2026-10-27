//! A stand-in Network Lock Manager (program 100021, v1/v3/v4). Every lock
//! request is granted and nothing is actually locked. Without an NLM, client
//! programs that take locks on the mount (editors, mail tools, package
//! managers) hang or fail; with this one they work, at the cost of no
//! protection between two clients writing the same file.

use crate::rpc::{Ctx, Program, Reject};
use crate::xdr::{Cur, Xdr};

pub const PROG: u32 = 100021;

const GRANTED: u32 = 0;

pub struct Nlm;

impl Program for Nlm {
    fn prog(&self) -> u32 {
        PROG
    }

    fn name(&self) -> &'static str {
        "nlm"
    }

    fn versions(&self) -> &[u32] {
        &[1, 3, 4]
    }

    fn proc_name(&self, _vers: u32, proc_num: u32) -> &'static str {
        match proc_num {
            0 => "NULL",
            1 => "TEST",
            2 => "LOCK",
            3 => "CANCEL",
            4 => "UNLOCK",
            5 => "GRANTED",
            20 => "SHARE",
            21 => "UNSHARE",
            22 => "NM_LOCK",
            23 => "FREE_ALL",
            _ => "?",
        }
    }

    fn call(&self, ctx: &Ctx, a: &mut Cur, o: &mut Xdr) -> Result<(), Reject> {
        // Every result starts by echoing the caller's cookie.
        let mut reply = |o: &mut Xdr, extra: Option<i32>| -> Result<(), Reject> {
            let cookie = a.opaque(1024).ok_or(Reject::GarbageArgs)?;
            o.opaque(cookie);
            o.u32(GRANTED);
            if let Some(seq) = extra {
                o.i32(seq);
            }
            Ok(())
        };
        match ctx.proc_num {
            0 | 23 => Ok(()),
            1..=5 | 22 => reply(o, None),
            20 | 21 => reply(o, Some(0)),
            // The *_MSG/*_RES variants need callbacks to the client.
            _ => Err(Reject::ProcUnavail),
        }
    }
}
