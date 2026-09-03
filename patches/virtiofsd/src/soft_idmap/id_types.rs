// Copyright 2024 Red Hat, Inc. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

/*!
 * Explicit types for host/guest UIDs/GIDs.
 *
 * The types provided by this module make it explicit whether some ID is valid on the host or in
 * the guest, and whether it is a UID or a GID.  Using them ensures proper and complete translation
 * between host and guest IDs, which would be difficult to prove when using primitive integer
 * types.
 */

use btree_range_map::{Measure, PartialEnum, RangePartialOrd};
use std::fmt::{self, Debug, Display, Formatter};
use std::ops::{Add, Sub};

/**
 * Common trait for all kinds of UIDs and GIDs.
 *
 * Its dependencies are:
 * - `Clone + Copy`: Must consist internally only of a plain integer, so must be copiable.
 * - `Debug + Display`: We want to easily print the type without deconstructing it.
 * - `From<Self::Inner>`: Must be constructable from its inner type (the raw numerical value).
 * - `Eq + PartialEq + Ord + PartialOrd`: Must be comparable, as would be expected from UIDs/GIDs.
 * - `Sub<Self>`: Must be able to calculate the offset of one ID compare to another, i.e. the
 *   length of an ID range.
 * - `Send + Sync`: Must be shareable between threads.
 * - `'static`: Required to construct error objects that can then be put into `io::Error`.
 */
pub trait Id:
    Clone
    + Copy
    + Debug
    + Display
    + Eq
    + From<Self::Inner>
    + Ord
    + PartialEq
    + PartialOrd
    + Send
    + Sub<Self>
    + Sync
    + 'static
{
    /**
     * Inner raw numerical type.
     *
     * Should be a primitive integer.  `Range<Self::Inner>` must be usable as the key for a
     * `btree_range_map::RangeMap`, hence the additional dependencies beyond `Clone + Copy`.
     */
    type Inner: Clone + Copy + Measure + PartialEnum + RangePartialOrd;

    /// Is this a root UID/GID?
    fn is_root(&self) -> bool;

    /// Get the raw numerical value.
    fn into_inner(self) -> Self::Inner;
}

/**
 * Trait designating a guest UID/GID.
 *
 * Must be able to add the length of an ID range of the corresponding host type, so we can map one
 * range to the other, e.g. like so:
 * ```
 * # use std::ops::Range;
 * # use virtiofsd::soft_idmap::{GuestUid, HostUid, Id};
 * # let guest_id_base: GuestUid = 5.into();
 * # let host_id_range: Range<HostUid> = (8.into()..13.into());
 * let guest_id_range = guest_id_base..(guest_id_base + (host_id_range.end - host_id_range.start));
 * # assert!(guest_id_range.start == guest_id_base);
 * # assert!(guest_id_range.end.into_inner() == 5 + 13 - 8);
 * ```
 *
 * Or:
 * ```
 * # use std::ops::Range;
 * # use virtiofsd::soft_idmap::{GuestUid, HostUid, Id};
 * # let guest_id_range: Range<GuestUid> = (8.into()..13.into());
 * # let host_id_range: Range<HostUid> = (21.into()..(21 + 13 - 8).into());
 * # let host_id_in_range: HostUid = 23.into();
 * let guest_id = guest_id_range.start + (host_id_in_range - host_id_range.start);
 * # assert!(guest_id.into_inner() == 8 + 23 - 21);
 * ```
 *
 * (Hence the `Add<<Self::HostType as Sub>::Output, Output = Self>` dependency.)
 */
pub trait GuestId: Id + Add<<Self::HostType as Sub>::Output, Output = Self> {
    /// Respective host UID or GID.
    type HostType: HostId;

    /// Plain identity mapping to the numerically equal host UID/GID.
    fn id_mapped(self) -> Self::HostType;
}

/**
 * Trait designating a host UID/GID.
 *
 * Must be able to add the length of an ID range of the corresponding guest type, so we can map one
 * range to the other, e.g. like so:
 * ```
 * # use std::ops::Range;
 * # use virtiofsd::soft_idmap::{GuestUid, HostUid, Id};
 * # let host_id_base: HostUid = 13.into();
 * # let guest_id_range: Range<GuestUid> = (21.into()..34.into());
 * let host_id_range = host_id_base..(host_id_base + (guest_id_range.end - guest_id_range.start));
 * # assert!(host_id_range.start == host_id_base);
 * # assert!(host_id_range.end.into_inner() == 13 + 34 - 21);
 * ```
 *
 * Or:
 * ```
 * # use std::ops::Range;
 * # use virtiofsd::soft_idmap::{GuestUid, HostUid, Id};
 * # let host_id_range: Range<HostUid> = (21.into()..34.into());
 * # let guest_id_range: Range<GuestUid> = (55.into()..(55 + 34 - 21).into());
 * # let guest_id_in_range: GuestUid = 66.into();
 * let host_id = host_id_range.start + (guest_id_in_range - guest_id_range.start);
 * # assert!(host_id.into_inner() == 21 + 66 - 55);
 * ```
 *
 * (Hence the `Add<<Self::GuestType as Sub>::Output, Output = Self>` dependency.)
 */
pub trait HostId: Id + Add<<Self::GuestType as Sub>::Output, Output = Self> {
    /// Respective guest UID or GID.
    type GuestType: GuestId;

    /// Plain identity mapping to the numerically equal guest UID/GID.
    fn id_mapped(self) -> Self::GuestType;
}

/// Internal: Implement various traits for ID types.
macro_rules! impl_ids {
    {
        $(
            $(#[$meta:meta])*
            $visibility:vis struct $t:ident<
                $opposite_name:tt = $opposite_type:ty,
                OffsetType = $offset_type:tt
            >($inner:ty): $variant_trait:tt;
        )*
    } => {
        $(
            $(#[$meta])*
            #[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
            #[repr(transparent)]
            pub struct $t($inner);

            impl From<$inner> for $t {
                fn from(id: $inner) -> Self {
                    $t(id)
                }
            }

            impl Id for $t {
                type Inner = $inner;

                fn is_root(&self) -> bool {
                    self.0 == 0
                }

                fn into_inner(self) -> $inner {
                    self.0
                }
            }

            impl $variant_trait for $t {
                type $opposite_name = $opposite_type;

                fn id_mapped(self) -> $opposite_type {
                    self.into_inner().into()
                }
            }

            impl Add<$offset_type> for $t {
                type Output = $t;

                fn add(self, rhs: $offset_type) -> $t {
                    (self.into_inner() + rhs.0).into()
                }
            }

            impl Sub<$t> for $t {
                type Output = $offset_type;

                fn sub(self, rhs: $t) -> $offset_type {
                    $offset_type(self.into_inner() - rhs.into_inner())
                }
            }

            impl Display for $t {
                fn fmt(&self, f: &mut Formatter) -> fmt::Result {
                    let inner = (*self).into_inner();
                    write!(f, "{inner}")
                }
            }
        )*
    };
}

/// Offset between two UIDs
pub struct UidOffset(u32);

/// Offset between two GIDs
pub struct GidOffset(u32);

impl_ids! {
    /// Guest UID type, i.e. a UID used in the guest.
    pub struct GuestUid<HostType = HostUid, OffsetType = UidOffset>(u32): GuestId;

    /// Guest GID type, i.e. a GID used in the guest.
    pub struct GuestGid<HostType = HostGid, OffsetType = GidOffset>(u32): GuestId;

    /// Host UID type, i.e. a UID valid on the host.
    pub struct HostUid<GuestType = GuestUid, OffsetType = UidOffset>(libc::uid_t): HostId;

    /// Host UID type, i.e. a GID valid on the host.
    pub struct HostGid<GuestType = GuestGid, OffsetType = GidOffset>(libc::gid_t): HostId;
}
