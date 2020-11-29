use super::cipher_suite::*;
use super::conn::*;
use super::curve::named_curve::*;
use super::errors::*;
use super::extension::extension_use_srtp::SRTPProtectionProfile;
use super::handshake::handshake_random::*;
use super::prf::*;

use util::Error;

use std::io::{BufWriter, Cursor};
use std::marker::{Send, Sync};
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::sync::Arc;

// State holds the dtls connection state and implements both encoding.BinaryMarshaler and encoding.BinaryUnmarshaler
pub(crate) struct State {
    pub(crate) local_epoch: Arc<AtomicU16>,
    pub(crate) remote_epoch: Arc<AtomicU16>,
    pub(crate) local_sequence_number: Arc<AtomicU64>, // uint48
    pub(crate) local_random: HandshakeRandom,
    pub(crate) remote_random: HandshakeRandom,
    pub(crate) master_secret: Vec<u8>,
    pub(crate) cipher_suite: Arc<Option<Box<dyn CipherSuite + Send + Sync>>>, // nil if a cipher_suite hasn't been chosen

    pub(crate) srtp_protection_profile: SRTPProtectionProfile, // Negotiated srtp_protection_profile
    pub(crate) peer_certificates: Vec<Vec<u8>>,

    pub(crate) is_client: bool,

    pub(crate) pre_master_secret: Vec<u8>,
    pub(crate) extended_master_secret: bool,

    pub(crate) named_curve: NamedCurve,
    pub(crate) local_keypair: Option<NamedCurveKeypair>,
    pub(crate) cookie: Vec<u8>,
    pub(crate) handshake_send_sequence: isize,
    pub(crate) handshake_recv_sequence: isize,
    pub(crate) server_name: String,
    pub(crate) remote_requested_certificate: bool, // Did we get a CertificateRequest
    pub(crate) local_certificates_verify: Vec<u8>, // cache CertificateVerify
    pub(crate) local_verify_data: Vec<u8>,         // cached VerifyData
    pub(crate) local_key_signature: Vec<u8>,       // cached keySignature
    pub(crate) peer_certificates_verified: bool,
    //pub(crate) replay_detector: Vec<Box<dyn ReplayDetector + Send + Sync>>,
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct SerializedState {
    local_epoch: u16,
    remote_epoch: u16,
    local_random: [u8; HANDSHAKE_RANDOM_LENGTH],
    remote_random: [u8; HANDSHAKE_RANDOM_LENGTH],
    cipher_suite_id: u16,
    master_secret: Vec<u8>,
    sequence_number: u64,
    srtp_protection_profile: u16,
    peer_certificates: Vec<Vec<u8>>,
    is_client: bool,
}

impl Clone for State {
    fn clone(&self) -> Self {
        let mut state = State::default();

        if let Ok(serialized) = self.serialize() {
            let _ = state.deserialize(&serialized);
        }

        state
    }
}

impl Default for State {
    fn default() -> Self {
        State {
            local_epoch: Arc::new(AtomicU16::new(0)),
            remote_epoch: Arc::new(AtomicU16::new(0)),
            local_sequence_number: Arc::new(AtomicU64::new(0)),
            local_random: HandshakeRandom::default(),
            remote_random: HandshakeRandom::default(),
            master_secret: vec![],
            cipher_suite: Arc::new(None), // nil if a cipher_suite hasn't been chosen

            srtp_protection_profile: SRTPProtectionProfile::Unsupported, // Negotiated srtp_protection_profile
            peer_certificates: vec![],

            is_client: false,

            pre_master_secret: vec![],
            extended_master_secret: false,

            named_curve: NamedCurve::Unsupported,
            local_keypair: None,
            cookie: vec![],
            handshake_send_sequence: 0,
            handshake_recv_sequence: 0,
            server_name: "".to_string(),
            remote_requested_certificate: false, // Did we get a CertificateRequest
            local_certificates_verify: vec![],   // cache CertificateVerify
            local_verify_data: vec![],           // cached VerifyData
            local_key_signature: vec![],         // cached keySignature
            peer_certificates_verified: false,
            //replay_detector: vec![],
        }
    }
}

impl State {
    fn serialize(&self) -> Result<SerializedState, Error> {
        let mut local_rand = vec![];
        {
            let mut writer = BufWriter::<&mut Vec<u8>>::new(local_rand.as_mut());
            self.local_random.marshal(&mut writer)?;
        }
        let mut remote_rand = vec![];
        {
            let mut writer = BufWriter::<&mut Vec<u8>>::new(remote_rand.as_mut());
            self.remote_random.marshal(&mut writer)?;
        }

        let mut local_random = [0u8; HANDSHAKE_RANDOM_LENGTH];
        let mut remote_random = [0u8; HANDSHAKE_RANDOM_LENGTH];

        local_random.copy_from_slice(&local_rand);
        remote_random.copy_from_slice(&remote_rand);

        let local_epoch = self.local_epoch.load(Ordering::Relaxed);
        let remote_epoch = self.remote_epoch.load(Ordering::Relaxed);
        let sequence_number = self.local_sequence_number.load(Ordering::Relaxed);
        let cipher_suite_id = match &*self.cipher_suite {
            Some(cipher_suite) => cipher_suite.id() as u16,
            None => return Err(ERR_CIPHER_SUITE_UNSET.clone()),
        };

        Ok(SerializedState {
            local_epoch,
            remote_epoch,
            local_random,
            remote_random,
            cipher_suite_id,
            master_secret: self.master_secret.clone(),
            sequence_number,
            srtp_protection_profile: self.srtp_protection_profile as u16,
            peer_certificates: self.peer_certificates.clone(),
            is_client: self.is_client,
        })
    }

    fn deserialize(&mut self, serialized: &SerializedState) -> Result<(), Error> {
        // Set epoch values
        self.local_epoch
            .store(serialized.local_epoch, Ordering::Relaxed);
        self.remote_epoch
            .store(serialized.remote_epoch, Ordering::Relaxed);
        self.local_sequence_number
            .store(serialized.sequence_number, Ordering::Relaxed);

        // Set random values
        let mut reader = Cursor::new(&serialized.local_random);
        self.local_random = HandshakeRandom::unmarshal(&mut reader)?;

        let mut reader = Cursor::new(&serialized.remote_random);
        self.remote_random = HandshakeRandom::unmarshal(&mut reader)?;

        self.is_client = serialized.is_client;

        // Set master secret
        self.master_secret = serialized.master_secret.clone();

        // Set cipher suite
        self.cipher_suite = Arc::new(Some(cipher_suite_for_id(
            serialized.cipher_suite_id.into(),
        )?));

        self.srtp_protection_profile = serialized.srtp_protection_profile.into();

        // Set remote certificate
        self.peer_certificates = serialized.peer_certificates.clone();

        Ok(())
    }

    pub async fn init_cipher_suite(&mut self) -> Result<(), Error> {
        if let Some(cipher_suite) = &*self.cipher_suite {
            if cipher_suite.is_initialized().await {
                return Ok(());
            }

            let mut local_random = vec![];
            {
                let mut writer = BufWriter::<&mut Vec<u8>>::new(local_random.as_mut());
                self.local_random.marshal(&mut writer)?;
            }
            let mut remote_random = vec![];
            {
                let mut writer = BufWriter::<&mut Vec<u8>>::new(remote_random.as_mut());
                self.remote_random.marshal(&mut writer)?;
            }

            if self.is_client {
                cipher_suite
                    .init(&self.master_secret, &local_random, &remote_random, true)
                    .await
            } else {
                cipher_suite
                    .init(&self.master_secret, &remote_random, &local_random, false)
                    .await
            }
        } else {
            Err(ERR_CIPHER_SUITE_UNSET.clone())
        }
    }

    // marshal_binary is a binary.BinaryMarshaler.marshal_binary implementation
    pub fn marshal_binary(&self) -> Result<Vec<u8>, Error> {
        let serialized = self.serialize()?;

        match bincode::serialize(&serialized) {
            Ok(enc) => Ok(enc),
            Err(err) => Err(Error::new(err.to_string())),
        }
    }

    // unmarshal_binary is a binary.BinaryUnmarshaler.UnmarshalBinary implementation
    pub async fn unmarshal_binary(&mut self, data: &[u8]) -> Result<(), Error> {
        let serialized: SerializedState = match bincode::deserialize(data) {
            Ok(dec) => dec,
            Err(err) => return Err(Error::new(err.to_string())),
        };
        self.deserialize(&serialized)?;
        self.init_cipher_suite().await?;

        Ok(())
    }

    // export_keying_material returns length bytes of exported key material in a new
    // slice as defined in RFC 5705.
    // This allows protocols to use DTLS for key establishment, but
    // then use some of the keying material for their own purposes
    pub fn export_keying_material(
        &self,
        label: &str,
        context: &[u8],
        length: usize,
    ) -> Result<Vec<u8>, Error> {
        if self.local_epoch.load(Ordering::Relaxed) == 0 {
            return Err(ERR_HANDSHAKE_IN_PROGRESS.clone());
        } else if !context.is_empty() {
            return Err(ERR_CONTEXT_UNSUPPORTED.clone());
        } else if INVALID_KEYING_LABELS.contains_key(label) {
            return Err(ERR_RESERVED_EXPORT_KEYING_MATERIAL.clone());
        }

        let mut local_random = vec![];
        {
            let mut writer = BufWriter::<&mut Vec<u8>>::new(local_random.as_mut());
            self.local_random.marshal(&mut writer)?;
        }
        let mut remote_random = vec![];
        {
            let mut writer = BufWriter::<&mut Vec<u8>>::new(remote_random.as_mut());
            self.remote_random.marshal(&mut writer)?;
        }

        let mut seed = vec![];
        if self.is_client {
            seed.extend_from_slice(&local_random);
            seed.extend_from_slice(&remote_random);
        } else {
            seed.extend_from_slice(&remote_random);
            seed.extend_from_slice(&local_random);
        }

        if let Some(cipher_suite) = &*self.cipher_suite {
            prf_p_hash(&self.master_secret, &seed, length, cipher_suite.hash_func())
        } else {
            Err(ERR_CIPHER_SUITE_UNSET.clone())
        }
    }
}