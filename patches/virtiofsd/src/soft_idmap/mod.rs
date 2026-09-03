// Copyright 2024 Red Hat, Inc. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

/*!
 * Facilities for mapping UIDs/GIDs within virtiofsd.
 *
 * This module provides various facilities to map UIDs/GIDs between host and guest, with separate
 * translation functions in either direction.
 */

pub mod cmdline;
pub mod id_types;

use crate::util::{other_io_error, ResultErrorContext};
use btree_range_map::RangeMap;
pub use id_types::{GuestGid, GuestId, GuestUid, HostGid, HostId, HostUid, Id};
use std::convert::TryFrom;
use std::fmt::{self, Display, Formatter};
use std::io;
use std::ops::{Add, Range, Sub};

/**
 * Provides mappings for UIDs or GIDs between host and guest.
 *
 * Each `IdMap` will only translate UIDs or GIDs, not both.  Translation in either direction (host
 * to guest, guest to host) is independent of the other direction, i.e. does not need to be
 * bijective (invertible).
 */
pub struct IdMap<Guest: GuestId<HostType = Host>, Host: HostId<GuestType = Guest>> {
    /// Guest-to-host mapping.
    guest_to_host: RangeMap<Guest::Inner, MapEntry<Guest, Host>>,
    /// Host-to-guest mapping.
    host_to_guest: RangeMap<Host::Inner, MapEntry<Host, Guest>>,
}

/**
 * Maps a range of IDs.
 *
 * Can be either UIDs or GIDs, and either host to guest or guest to host.
 */
#[derive(Clone, Debug, PartialEq)]
enum MapEntry<Source: Id, Target: Id> {
    /// Squash a range of IDs onto a single one.
    Squash {
        /// Range of source IDs.
        from: Range<Source>,
        /// Single target ID.
        to: Target,
    },

    /// 1:1 map a range of IDs to another range (of the same length).
    Range {
        /// Range of source IDs.
        from: Range<Source>,
        /// First ID in the target range (i.e. mapping for `from.start`).
        to_base: Target,
    },

    /// Disallow using this ID range: Return an error.
    Fail {
        /// Range of source IDs.
        from: Range<Source>,
    },
}

#[derive(Clone, Debug)]
pub enum MapError<Source: Id> {
    ExplicitFailMapping { id: Source },
}

impl<Guest, Host> IdMap<Guest, Host>
where
    Guest: GuestId<HostType = Host>,
    Host: HostId<GuestType = Guest>,
{
    /**
     * Create an empty map.
     *
     * Note that unmapped ranges default to identity mapping, i.e. an empty map will map everything
     * to itself (numerically speaking).
     */
    pub fn empty() -> Self {
        IdMap {
            guest_to_host: RangeMap::new(),
            host_to_guest: RangeMap::new(),
        }
    }

    /// Map a guest UID/GID to one in the host domain.
    pub fn map_guest(&self, guest_id: Guest) -> Result<Host, MapError<Guest>> {
        self.guest_to_host
            .get(guest_id.into_inner())
            .map(|e| e.map(guest_id))
            .unwrap_or(Ok(guest_id.id_mapped()))
    }

    /// Map a host UID/GID to one in the guest domain.
    pub fn map_host(&self, host_id: Host) -> Result<Guest, MapError<Host>> {
        self.host_to_guest
            .get(host_id.into_inner())
            .map(|e| e.map(host_id))
            .unwrap_or(Ok(host_id.id_mapped()))
    }

    /**
     * Add a new mapping.
     *
     * Internal helper for [`Self::push_guest_to_host()`] and [`Self::push_host_to_guest()`].
     *
     * `map` points to either `self.guest_to_host` or `self.host_to_guest`.  `map_name` should be
     * `"Guest-to-host"` or `"Host-to-guest"` accordingly, and is only used to generate potential
     * error messages.
     */
    fn do_push<Source, Target>(
        map: &mut RangeMap<Source::Inner, MapEntry<Source, Target>>,
        map_name: &str,
        entry: MapEntry<Source, Target>,
    ) -> io::Result<()>
    where
        Source: Id + Sub<Source>,
        Target: Id + Add<<Source as Sub>::Output, Output = Target>,
    {
        let wrapped_range = entry.source_range();
        let inner_range = Range {
            start: wrapped_range.start.into_inner(),
            end: wrapped_range.end.into_inner(),
        };
        if map.intersects(inner_range.clone()) {
            return Err(other_io_error(format!(
                "{map_name} mapping '{entry}' intersects previously added entry"
            )));
        }

        map.insert(inner_range, entry);
        Ok(())
    }

    /**
     * Add a new mapping of guest IDs to host ID(s).
     *
     * Internal helper for [`Self as
     * TryFrom<Vec<cmdline::IdMap>>`](`Self#impl-TryFrom<Vec<IdMap>>-for-IdMap<Guest,+Host>`).
     */
    fn push_guest_to_host(&mut self, entry: MapEntry<Guest, Host>) -> io::Result<()> {
        Self::do_push(&mut self.guest_to_host, "Guest-to-host", entry)
    }

    /**
     * Add a new mapping of host IDs to guest ID(s).
     *
     * Internal helper for [`Self as
     * TryFrom<Vec<cmdline::IdMap>>`](`Self#impl-TryFrom<Vec<IdMap>>-for-IdMap<Guest,+Host>`).
     */
    fn push_host_to_guest(&mut self, entry: MapEntry<Host, Guest>) -> io::Result<()> {
        Self::do_push(&mut self.host_to_guest, "Host-to-guest", entry)
    }
}

impl<Source: Id, Target: Id> MapEntry<Source, Target>
where
    Source: Sub<Source>,
    Target: Add<<Source as Sub>::Output, Output = Target>,
{
    /// Map an element from the source domain into the target domain.
    fn map(&self, id: Source) -> Result<Target, MapError<Source>> {
        match self {
            MapEntry::Squash { from, to } => {
                assert!(from.contains(&id));
                Ok(*to)
            }

            MapEntry::Range { from, to_base } => {
                assert!(from.contains(&id));
                Ok(*to_base + (id - from.start))
            }

            MapEntry::Fail { from } => {
                assert!(from.contains(&id));
                Err(MapError::ExplicitFailMapping { id })
            }
        }
    }

    /// Return the source ID range.
    fn source_range(&self) -> &Range<Source> {
        match self {
            MapEntry::Squash { from, to: _ } => from,
            MapEntry::Range { from, to_base: _ } => from,
            MapEntry::Fail { from } => from,
        }
    }
}

impl<Source: Id> Display for MapError<Source> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            MapError::ExplicitFailMapping { id } => {
                write!(f, "Use of ID {id} has been configured to fail")
            }
        }
    }
}

impl<Source: Id> std::error::Error for MapError<Source> {}

impl<Source: Id> From<MapError<Source>> for io::Error {
    fn from(err: MapError<Source>) -> Self {
        io::Error::new(io::ErrorKind::PermissionDenied, err)
    }
}

impl<Source: Id, Target: Id> Display for MapEntry<Source, Target>
where
    Source: Sub<Source>,
    Target: Add<<Source as Sub>::Output, Output = Target>,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            MapEntry::Squash { from, to } => {
                write!(f, "squash [{}, {}) to {to}", from.start, from.end)
            }
            MapEntry::Range { from, to_base } => {
                write!(
                    f,
                    "map [{}, {}) to [{to_base}, {})",
                    from.start,
                    from.end,
                    *to_base + (from.end - from.start)
                )
            }
            MapEntry::Fail { from } => {
                write!(f, "fail [{}, {})", from.start, from.end)
            }
        }
    }
}

fn id_range_from_u32<I, P: Display>(base: u32, count: u32, param: P) -> io::Result<Range<I>>
where
    u32: Into<I>,
{
    let start: I = base.into();
    let end: I = base
        .checked_add(count)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Parameter {param}: Range overflow"),
            )
        })?
        .into();
    Ok(start..end)
}

impl<Guest, Host> TryFrom<Vec<cmdline::IdMap>> for IdMap<Guest, Host>
where
    Guest: GuestId<HostType = Host> + From<u32>,
    Host: HostId<GuestType = Guest> + From<u32>,
{
    type Error = io::Error;

    /// Convert from the command line representation to our runtime object.
    fn try_from(cmdline: Vec<cmdline::IdMap>) -> io::Result<Self> {
        let mut map = IdMap::empty();

        for entry in cmdline {
            match entry {
                cmdline::IdMap::Guest {
                    from_guest,
                    to_host,
                    count,
                } => map
                    .push_guest_to_host(MapEntry::Range {
                        from: id_range_from_u32(from_guest, count, &entry)?,
                        to_base: to_host.into(),
                    })
                    .err_context(|| entry)?,

                cmdline::IdMap::Host {
                    from_host,
                    to_guest,
                    count,
                } => map
                    .push_host_to_guest(MapEntry::Range {
                        from: id_range_from_u32(from_host, count, &entry)?,
                        to_base: to_guest.into(),
                    })
                    .err_context(|| entry)?,

                cmdline::IdMap::SquashGuest {
                    from_guest,
                    to_host,
                    count,
                } => map
                    .push_guest_to_host(MapEntry::Squash {
                        from: id_range_from_u32(from_guest, count, &entry)?,
                        to: to_host.into(),
                    })
                    .err_context(|| entry)?,

                cmdline::IdMap::SquashHost {
                    from_host,
                    to_guest,
                    count,
                } => map
                    .push_host_to_guest(MapEntry::Squash {
                        from: id_range_from_u32(from_host, count, &entry)?,
                        to: to_guest.into(),
                    })
                    .err_context(|| entry)?,

                cmdline::IdMap::Bidirectional { guest, host, count } => {
                    map.push_guest_to_host(MapEntry::Range {
                        from: id_range_from_u32(guest, count, &entry)?,
                        to_base: host.into(),
                    })
                    .err_context(|| &entry)?;

                    map.push_host_to_guest(MapEntry::Range {
                        from: id_range_from_u32(host, count, &entry)?,
                        to_base: guest.into(),
                    })
                    .err_context(|| &entry)?;
                }

                cmdline::IdMap::ForbidGuest { from_guest, count } => {
                    map.push_guest_to_host(MapEntry::Fail {
                        from: (from_guest.into())..((from_guest + count).into()),
                    })
                    .err_context(|| &entry)?;
                }
            }
        }

        Ok(map)
    }
}
