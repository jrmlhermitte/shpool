// Copyright 2024 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{
    io,
    os::unix::net::UnixStream,
    sync::{Arc, Mutex},
};

use anyhow::{anyhow, Context};
use tracing::info;

use crate::{
    config,
    daemon::pager::{Pager, PagerCtl},
    protocol, tty,
};

/// Showers know how to show the message of the day.
#[derive(Debug, Clone)]
pub struct DailyMessenger {
    motd_resolver: motd::Resolver,
    mode: config::MotdDisplayMode,
    args: Option<Vec<String>>,
}

impl DailyMessenger {
    /// Make a new Shower.
    pub fn new(mode: config::MotdDisplayMode, args: Option<Vec<String>>) -> anyhow::Result<Self> {
        Ok(DailyMessenger {
            motd_resolver: motd::Resolver::new(motd::PamMotdResolutionStrategy::Auto)
                .context("creating motd resolver")?,
            mode,
            args,
        })
    }

    pub fn dump<W: io::Write>(
        &self,
        mut stream: W,
        term_db: &termini::TermInfo,
    ) -> anyhow::Result<()> {
        assert!(matches!(self.mode, config::MotdDisplayMode::Dump));

        let raw_motd_value = self.raw_motd_value(term_db)?;

        let chunk =
            protocol::Chunk { kind: protocol::ChunkKind::Data, buf: raw_motd_value.as_slice() };

        chunk.write_to(&mut stream).context("dumping motd")
    }

    /// Display the motd in a pager. Callers should do a downcast error
    /// check for PagerError::ClientHangup as if they had called
    /// Pager::display directly.
    pub fn display_in_pager(
        &self,
        // The client connection on which to display the pager.
        client_stream: &mut UnixStream,
        // The session to associate this pager with for SIGWINCH purposes.
        ctl_slot: Arc<Mutex<Option<PagerCtl>>>,
        // The size of the tty to start off with
        init_tty_size: tty::Size,
    ) -> anyhow::Result<tty::Size> {
        let pager_bin = if let config::MotdDisplayMode::Pager { bin } = &self.mode {
            bin
        } else {
            return Err(anyhow!("internal error: wrong mode to display in pager"));
        };

        info!("displaying motd in pager '{}'", pager_bin);

        let motd_value = self.motd_value()?;

        let pager = Pager::new(pager_bin.to_string());

        pager.display(client_stream, ctl_slot, init_tty_size, motd_value.as_str())
    }

    fn motd_value(&self) -> anyhow::Result<String> {
        self.motd_resolver
            .value(match &self.args {
                Some(args) => {
                    let mut args = args.clone();
                    // On debian based systems we need to set noupdate in order to get
                    // the motd from userspace. It should be ignored on non-debian systems.
                    if !args.iter().any(|a| a == "noupdate") {
                        args.push(String::from("noupdate"));
                    }
                    motd::ArgResolutionStrategy::Exact(args)
                }
                None => motd::ArgResolutionStrategy::Auto,
            })
            .context("resolving motd")
    }

    fn raw_motd_value(&self, term_db: &termini::TermInfo) -> anyhow::Result<Vec<u8>> {
        let motd_value = self.motd_value()?;
        Self::convert_to_raw(term_db, &motd_value)
    }

    /// Convert the given motd into a byte buffer suitable to be written to the
    /// terminal. The only real transformation we perform is injecting carrage
    /// returns after newlines.
    fn convert_to_raw(term_db: &termini::TermInfo, motd: &str) -> anyhow::Result<Vec<u8>> {
        let carrage_return_code = term_db
            .raw_string_cap(termini::StringCapability::CarriageReturn)
            .ok_or(anyhow!("no carrage return code"))?;

        let mut buf: Vec<u8> = vec![];

        let lines = motd.split('\n');
        for line in lines {
            buf.extend(line.as_bytes());
            buf.push(b'\n');
            buf.extend(carrage_return_code);
        }

        Ok(buf)
    }
}
