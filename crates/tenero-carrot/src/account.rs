//! An account: its secrets (from one master secret), its public keys and its addresses (Carrot 5.2, 5.3, 6.1).
//!
//! The access tiers of Carrot 5.4 are the secrets each holds:
//! * **generate-address** (`s_ga` and the public keys): can make subaddresses, nothing else;
//! * **view-received** (`k_v`, `s_ga`): finds incoming payments ([`ViewReceived`]);
//! * **view-all** (`s_vb`): also finds its own change and self-sends, so it sees outgoing payments too ([`ViewAll`]);
//! * **master** (`s_m`): can also spend ([`AccountSecrets`]).

use curve25519_dalek::edwards::EdwardsPoint;
use curve25519_dalek::scalar::Scalar;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::derive::*;
use crate::points::{compress, decompress};
use crate::{CarrotError, PaymentId, NULL_PAYMENT_ID};

/// Where an output can be sent: an address's two public keys, whether it is a subaddress, and its payment ID (all zero
/// unless it is an integrated address).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Destination {
    pub spend_pubkey: [u8; 32],
    pub view_pubkey: [u8; 32],
    pub is_subaddress: bool,
    pub payment_id: PaymentId,
}

/// An address index `(major, minor)`. `(0, 0)` is the main address.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AddressIndex {
    pub major: u32,
    pub minor: u32,
}

impl AddressIndex {
    pub const MAIN: AddressIndex = AddressIndex { major: 0, minor: 0 };

    pub fn is_subaddress(&self) -> bool {
        *self != AddressIndex::MAIN
    }
}

/// The public side of an account: what makes its addresses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccountPublic {
    /// `K_s = k_gi G + k_ps T`, also the main address's spend key.
    pub spend_pubkey: [u8; 32],
    /// `K_v = k_v K_s`: the account view key that subaddresses are made from (not the main address's view key).
    pub view_pubkey: [u8; 32],
    /// `K^0_v = k_v G`: the main address's view key.
    pub main_view_pubkey: [u8; 32],
}

impl AccountPublic {
    pub fn main_address(&self) -> Destination {
        Destination {
            spend_pubkey: self.spend_pubkey,
            view_pubkey: self.main_view_pubkey,
            is_subaddress: false,
            payment_id: NULL_PAYMENT_ID,
        }
    }

    /// The main address with a payment ID (`make_carrot_integrated_address_v1`).
    pub fn integrated_address(&self, payment_id: PaymentId) -> Destination {
        Destination {
            payment_id,
            ..self.main_address()
        }
    }
}

/// The scalar of subaddress `index` (`k^j_subscal`); the main address's is one.
pub fn subaddress_scalar(
    public: &AccountPublic,
    s_generate_address: &[u8; 32],
    index: AddressIndex,
) -> Scalar {
    if !index.is_subaddress() {
        return Scalar::ONE;
    }
    let mut p1 = make_address_index_preimage_1(s_generate_address, index.major, index.minor);
    let mut p2 = make_address_index_preimage_2(
        &p1,
        index.major,
        index.minor,
        &public.spend_pubkey,
        &public.view_pubkey,
    );
    let s = make_subaddress_scalar(&p2, &public.spend_pubkey);
    p1.zeroize();
    p2.zeroize();
    s
}

/// Subaddress `index` (`make_carrot_subaddress_v1`): `(k^j_subscal K_s, k^j_subscal K_v)`. Index `(0, 0)` is refused, as
/// in C++: that is the main address, which has a different view key.
pub fn subaddress(
    public: &AccountPublic,
    s_generate_address: &[u8; 32],
    index: AddressIndex,
) -> Result<Destination, CarrotError> {
    if !index.is_subaddress() {
        return Err(CarrotError::BadAddressType(
            "index (0, 0) is the main address, not a subaddress",
        ));
    }
    let s = subaddress_scalar(public, s_generate_address, index);
    let spend = decompress(&public.spend_pubkey).ok_or(CarrotError::InvalidPoint)?;
    let view = decompress(&public.view_pubkey).ok_or(CarrotError::InvalidPoint)?;
    Ok(Destination {
        spend_pubkey: compress(&(spend * s)),
        view_pubkey: compress(&(view * s)),
        is_subaddress: true,
        payment_id: NULL_PAYMENT_ID,
    })
}

/// The address of `index`: the main address or a subaddress.
pub fn address(
    public: &AccountPublic,
    s_generate_address: &[u8; 32],
    index: AddressIndex,
) -> Result<Destination, CarrotError> {
    if index.is_subaddress() {
        subaddress(public, s_generate_address, index)
    } else {
        Ok(public.main_address())
    }
}

/// The view-received tier: the incoming view key and the generate-address secret.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct ViewReceived {
    pub k_view_incoming: Scalar,
    pub s_generate_address: [u8; 32],
    #[zeroize(skip)]
    pub public: AccountPublic,
}

/// The view-all tier: the view-balance secret and the partial spend key `K_ps = k_ps T`. With them it finds incoming
/// payments, its own change and self-sends, and computes key images (`k_gi` follows from `s_vb` and `K_ps`), so it also
/// sees which outputs are spent. It cannot spend: that needs `k_ps`.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct ViewAll {
    pub s_view_balance: [u8; 32],
    #[zeroize(skip)]
    pub partial_spend_pubkey: [u8; 32],
    #[zeroize(skip)]
    pub public: AccountPublic,
}

impl ViewAll {
    /// From `s_vb` and `K_ps`. The account spend key follows: `K_s = k_gi G + K_ps`.
    pub fn new(
        s_view_balance: [u8; 32],
        partial_spend_pubkey: [u8; 32],
    ) -> Result<ViewAll, CarrotError> {
        let k_ps_pub = decompress(&partial_spend_pubkey).ok_or(CarrotError::InvalidPoint)?;
        let k_gi = make_generateimage_key(
            &make_generateimage_preimage(&s_view_balance),
            &partial_spend_pubkey,
        );
        let k_v = make_viewincoming_key(&s_view_balance);
        let spend = EdwardsPoint::mul_base(&k_gi) + k_ps_pub;
        let public = AccountPublic {
            spend_pubkey: compress(&spend),
            view_pubkey: compress(&(spend * k_v)),
            main_view_pubkey: compress(&EdwardsPoint::mul_base(&k_v)),
        };
        Ok(ViewAll {
            s_view_balance,
            partial_spend_pubkey,
            public,
        })
    }

    pub fn k_generate_image(&self) -> Scalar {
        make_generateimage_key(
            &make_generateimage_preimage(&self.s_view_balance),
            &self.partial_spend_pubkey,
        )
    }

    pub fn view_received(&self) -> ViewReceived {
        ViewReceived {
            k_view_incoming: make_viewincoming_key(&self.s_view_balance),
            s_generate_address: make_generateaddress_secret(&self.s_view_balance),
            public: self.public,
        }
    }
}

/// Every secret of an account, derived from its master secret `s_m` (Carrot 5.2).
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct AccountSecrets {
    pub s_master: [u8; 32],
    pub k_prove_spend: Scalar,
    pub s_view_balance: [u8; 32],
    pub s_generate_image_preimage: [u8; 32],
    pub k_generate_image: Scalar,
    pub k_view_incoming: Scalar,
    pub s_generate_address: [u8; 32],
    #[zeroize(skip)]
    pub public: AccountPublic,
}

impl AccountSecrets {
    pub fn from_master(s_master: &[u8; 32]) -> AccountSecrets {
        let k_ps = make_provespend_key(s_master);
        let s_vb = make_viewbalance_secret(s_master);
        let s_gp = make_generateimage_preimage(&s_vb);
        let k_gi = make_generateimage_key(&s_gp, &make_partial_spend_pubkey(&k_ps));
        let k_v = make_viewincoming_key(&s_vb);
        let s_ga = make_generateaddress_secret(&s_vb);
        let spend_point = crate::points::scalar_mult_gt(&k_gi, &k_ps);
        let public = AccountPublic {
            spend_pubkey: compress(&spend_point),
            view_pubkey: compress(&(spend_point * k_v)),
            main_view_pubkey: compress(&EdwardsPoint::mul_base(&k_v)),
        };
        AccountSecrets {
            s_master: *s_master,
            k_prove_spend: k_ps,
            s_view_balance: s_vb,
            s_generate_image_preimage: s_gp,
            k_generate_image: k_gi,
            k_view_incoming: k_v,
            s_generate_address: s_ga,
            public,
        }
    }

    pub fn view_all(&self) -> ViewAll {
        ViewAll {
            s_view_balance: self.s_view_balance,
            partial_spend_pubkey: make_partial_spend_pubkey(&self.k_prove_spend),
            public: self.public,
        }
    }

    pub fn view_received(&self) -> ViewReceived {
        ViewReceived {
            k_view_incoming: self.k_view_incoming,
            s_generate_address: self.s_generate_address,
            public: self.public,
        }
    }

    pub fn address(&self, index: AddressIndex) -> Result<Destination, CarrotError> {
        address(&self.public, &self.s_generate_address, index)
    }
}
