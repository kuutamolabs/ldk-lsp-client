// This file is Copyright its original authors, visible in version control
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

//! Contains the main LSPS2 server-side object, [`LSPS2ServiceHandler`].

use crate::events::EventQueue;
use crate::lsps0::msgs::{ProtocolMessageHandler, RequestId};
use crate::lsps2::event::LSPS2ServiceEvent;
use crate::lsps2::utils::{compute_opening_fee, is_valid_opening_fee_params};
use crate::message_queue::MessageQueue;
use crate::prelude::{HashMap, String, ToString, Vec};
use crate::sync::{Arc, Mutex, RwLock};
use crate::{events::Event, lsps0::msgs::ResponseError};

use lightning::ln::channelmanager::{AChannelManager, InterceptId};
use lightning::ln::msgs::{ErrorAction, LightningError};
use lightning::ln::ChannelId;
use lightning::util::errors::APIError;
use lightning::util::logger::Level;

use bitcoin::secp256k1::PublicKey;

use core::ops::Deref;

use crate::lsps2::msgs::{
    BuyRequest, BuyResponse, GetInfoRequest, GetInfoResponse, LSPS2Message, LSPS2Request,
    LSPS2Response, OpeningFeeParams, RawOpeningFeeParams,
    LSPS2_BUY_REQUEST_INVALID_OPENING_FEE_PARAMS_ERROR_CODE,
    LSPS2_BUY_REQUEST_PAYMENT_SIZE_TOO_LARGE_ERROR_CODE,
    LSPS2_BUY_REQUEST_PAYMENT_SIZE_TOO_SMALL_ERROR_CODE,
    LSPS2_GET_INFO_REQUEST_UNRECOGNIZED_OR_STALE_TOKEN_ERROR_CODE,
};

/// Server-side configuration options for JIT channels.
#[derive(Clone, Debug)]
pub struct LSPS2ServiceConfig {
    /// Used to calculate the promise for channel parameters supplied to clients.
    ///
    /// Note: If this changes then old promises given out will be considered invalid.
    pub promise_secret: [u8; 32],
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
struct InterceptedHTLC {
    intercept_id: InterceptId,
    expected_outbound_amount_msat: u64,
}

struct ChannelStateError(String);

impl From<ChannelStateError> for LightningError {
    fn from(value: ChannelStateError) -> Self {
        LightningError { err: value.0, action: ErrorAction::IgnoreAndLog(Level::Info) }
    }
}

#[derive(PartialEq, Debug)]
enum OutboundJITChannelState {
    AwaitingPayment {
        min_fee_msat: u64,
        proportional_fee: u32,
        min_payment_size_msat: u64,
        max_payment_size_msat: u64,
        htlcs: Vec<InterceptedHTLC>,
        payment_size_msat: Option<u64>,
    },
    PendingChannelOpen {
        htlcs: Vec<InterceptedHTLC>,
        opening_fee_msat: u64,
        amt_to_forward_msat: u64,
    },
    ChannelReady {
        htlcs: Vec<InterceptedHTLC>,
        amt_to_forward_msat: u64,
    },
}

impl OutboundJITChannelState {
    fn new(payment_size_msat: Option<u64>, opening_fee_params: OpeningFeeParams) -> Self {
        OutboundJITChannelState::AwaitingPayment {
            min_fee_msat: opening_fee_params.min_fee_msat,
            proportional_fee: opening_fee_params.proportional,
            min_payment_size_msat: opening_fee_params.min_payment_size_msat,
            max_payment_size_msat: opening_fee_params.max_payment_size_msat,
            htlcs: vec![],
            payment_size_msat,
        }
    }

    fn htlc_intercepted(&self, htlc: InterceptedHTLC) -> Result<Self, ChannelStateError> {
        match self {
            OutboundJITChannelState::AwaitingPayment {
                htlcs,
                payment_size_msat,
                min_payment_size_msat,
                max_payment_size_msat,
                min_fee_msat,
                proportional_fee,
            } => {
                let mut htlcs = htlcs.clone();
                htlcs.push(htlc);

                let total_expected_outbound_amount_msat =
                    htlcs.iter().map(|htlc| htlc.expected_outbound_amount_msat).sum();

                let (expected_payment_size_msat, mpp_mode) =
                    if let Some(payment_size_msat) = payment_size_msat {
                        (*payment_size_msat, true)
                    } else {
                        debug_assert_eq!(htlcs.len(), 1);
                        if htlcs.len() != 1 {
                            return Err(ChannelStateError(
                                format!("Paying via multiple HTLCs is disallowed in \"no-MPP+var-invoice\" mode.")
                            ));
                        }
                        (total_expected_outbound_amount_msat, false)
                    };

                if expected_payment_size_msat < *min_payment_size_msat
                    || expected_payment_size_msat > *max_payment_size_msat
                {
                    return Err(ChannelStateError(
                            format!("Payment size violates our limits: expected_payment_size_msat = {}, min_payment_size_msat = {}, max_payment_size_msat = {}",
                                    expected_payment_size_msat,
                                    min_payment_size_msat,
                                    max_payment_size_msat
                            )));
                }

                let opening_fee_msat = compute_opening_fee(
                    expected_payment_size_msat,
                    *min_fee_msat,
                    (*proportional_fee).into(),
                ).ok_or(ChannelStateError(
                    format!("Could not compute valid opening fee with min_fee_msat = {}, proportional = {}, and expected_payment_size_msat = {}",
                        min_fee_msat,
                        proportional_fee,
                        expected_payment_size_msat
                    )
                ))?;

                let amt_to_forward_msat =
                    expected_payment_size_msat.saturating_sub(opening_fee_msat);

                // Go ahead and open the channel if we intercepted sufficient HTLCs.
                if total_expected_outbound_amount_msat >= expected_payment_size_msat
                    && amt_to_forward_msat > 0
                {
                    Ok(OutboundJITChannelState::PendingChannelOpen {
                        htlcs,
                        opening_fee_msat,
                        amt_to_forward_msat,
                    })
                } else {
                    if mpp_mode {
                        Ok(OutboundJITChannelState::AwaitingPayment {
                            min_fee_msat: *min_fee_msat,
                            proportional_fee: *proportional_fee,
                            htlcs,
                            payment_size_msat: *payment_size_msat,
                            min_payment_size_msat: *min_payment_size_msat,
                            max_payment_size_msat: *max_payment_size_msat,
                        })
                    } else {
                        Err(ChannelStateError(
                            "Intercepted HTLC is too small to pay opening fee".to_string(),
                        ))
                    }
                }
            }
            state => Err(ChannelStateError(format!(
                "Intercepted HTLC when JIT Channel was in state: {:?}",
                state
            ))),
        }
    }

    fn channel_ready(&self) -> Result<Self, ChannelStateError> {
        match self {
            OutboundJITChannelState::PendingChannelOpen { htlcs, amt_to_forward_msat, .. } => {
                Ok(OutboundJITChannelState::ChannelReady {
                    htlcs: htlcs.clone(),
                    amt_to_forward_msat: *amt_to_forward_msat,
                })
            }
            state => Err(ChannelStateError(format!(
                "Channel ready received when JIT Channel was in state: {:?}",
                state
            ))),
        }
    }
}

struct OutboundJITChannel {
    state: OutboundJITChannelState,
    user_channel_id: u128,
}

impl OutboundJITChannel {
    fn new(
        payment_size_msat: Option<u64>, opening_fee_params: OpeningFeeParams, user_channel_id: u128,
    ) -> Self {
        Self {
            user_channel_id,
            state: OutboundJITChannelState::new(payment_size_msat, opening_fee_params),
        }
    }

    fn htlc_intercepted(
        &mut self, htlc: InterceptedHTLC,
    ) -> Result<Option<(u64, u64)>, LightningError> {
        self.state = self.state.htlc_intercepted(htlc)?;

        match &self.state {
            OutboundJITChannelState::AwaitingPayment { .. } => {
                // TODO: log that we received an htlc but are still awaiting payment
                Ok(None)
            }
            OutboundJITChannelState::PendingChannelOpen {
                opening_fee_msat,
                amt_to_forward_msat,
                ..
            } => Ok(Some((*opening_fee_msat, *amt_to_forward_msat))),
            impossible_state => Err(LightningError {
                err: format!(
                    "Impossible state transition during htlc_intercepted to {:?}",
                    impossible_state
                ),
                action: ErrorAction::IgnoreAndLog(Level::Info),
            }),
        }
    }

    fn channel_ready(&mut self) -> Result<(Vec<InterceptedHTLC>, u64), LightningError> {
        self.state = self.state.channel_ready()?;

        match &self.state {
            OutboundJITChannelState::ChannelReady { htlcs, amt_to_forward_msat } => {
                Ok((htlcs.clone(), *amt_to_forward_msat))
            }
            impossible_state => Err(LightningError {
                err: format!(
                    "Impossible state transition during channel_ready to {:?}",
                    impossible_state
                ),
                action: ErrorAction::IgnoreAndLog(Level::Info),
            }),
        }
    }
}

struct PeerState {
    outbound_channels_by_intercept_scid: HashMap<u64, OutboundJITChannel>,
    intercept_scid_by_user_channel_id: HashMap<u128, u64>,
    pending_requests: HashMap<RequestId, LSPS2Request>,
}

impl PeerState {
    fn new() -> Self {
        let outbound_channels_by_intercept_scid = HashMap::new();
        let pending_requests = HashMap::new();
        let intercept_scid_by_user_channel_id = HashMap::new();
        Self {
            outbound_channels_by_intercept_scid,
            pending_requests,
            intercept_scid_by_user_channel_id,
        }
    }

    fn insert_outbound_channel(&mut self, intercept_scid: u64, channel: OutboundJITChannel) {
        self.outbound_channels_by_intercept_scid.insert(intercept_scid, channel);
    }
}

/// The main object allowing to send and receive LSPS2 messages.
pub struct LSPS2ServiceHandler<CM: Deref + Clone>
where
    CM::Target: AChannelManager,
{
    channel_manager: CM,
    pending_messages: Arc<MessageQueue>,
    pending_events: Arc<EventQueue>,
    per_peer_state: RwLock<HashMap<PublicKey, Mutex<PeerState>>>,
    peer_by_intercept_scid: RwLock<HashMap<u64, PublicKey>>,
    config: LSPS2ServiceConfig,
}

impl<CM: Deref + Clone> LSPS2ServiceHandler<CM>
where
    CM::Target: AChannelManager,
{
    /// Constructs a `LSPS2ServiceHandler`.
    pub(crate) fn new(
        pending_messages: Arc<MessageQueue>, pending_events: Arc<EventQueue>, channel_manager: CM,
        config: LSPS2ServiceConfig,
    ) -> Self {
        Self {
            pending_messages,
            pending_events,
            per_peer_state: RwLock::new(HashMap::new()),
            peer_by_intercept_scid: RwLock::new(HashMap::new()),
            channel_manager,
            config,
        }
    }

    /// Used by LSP to inform a client requesting a JIT Channel the token they used is invalid.
    ///
    /// Should be called in response to receiving a [`LSPS2ServiceEvent::GetInfo`] event.
    ///
    /// [`LSPS2ServiceEvent::GetInfo`]: crate::lsps2::event::LSPS2ServiceEvent::GetInfo
    pub fn invalid_token_provided(
        &self, counterparty_node_id: &PublicKey, request_id: RequestId,
    ) -> Result<(), APIError> {
        let outer_state_lock = self.per_peer_state.read().unwrap();

        match outer_state_lock.get(counterparty_node_id) {
            Some(inner_state_lock) => {
                let mut peer_state = inner_state_lock.lock().unwrap();

                match peer_state.pending_requests.remove(&request_id) {
                    Some(LSPS2Request::GetInfo(_)) => {
                        let response = LSPS2Response::GetInfoError(ResponseError {
                            code: LSPS2_GET_INFO_REQUEST_UNRECOGNIZED_OR_STALE_TOKEN_ERROR_CODE,
                            message: "an unrecognized or stale token was provided".to_string(),
                            data: None,
                        });
                        self.enqueue_response(counterparty_node_id, request_id, response);
                        Ok(())
                    }
                    _ => Err(APIError::APIMisuseError {
                        err: format!(
                            "No pending get_info request for request_id: {:?}",
                            request_id
                        ),
                    }),
                }
            }
            None => Err(APIError::APIMisuseError {
                err: format!("No state for the counterparty exists: {:?}", counterparty_node_id),
            }),
        }
    }

    /// Used by LSP to provide fee parameters to a client requesting a JIT Channel.
    ///
    /// Should be called in response to receiving a [`LSPS2ServiceEvent::GetInfo`] event.
    ///
    /// [`LSPS2ServiceEvent::GetInfo`]: crate::lsps2::event::LSPS2ServiceEvent::GetInfo
    pub fn opening_fee_params_generated(
        &self, counterparty_node_id: &PublicKey, request_id: RequestId,
        opening_fee_params_menu: Vec<RawOpeningFeeParams>,
    ) -> Result<(), APIError> {
        let outer_state_lock = self.per_peer_state.read().unwrap();

        match outer_state_lock.get(counterparty_node_id) {
            Some(inner_state_lock) => {
                let mut peer_state = inner_state_lock.lock().unwrap();

                match peer_state.pending_requests.remove(&request_id) {
                    Some(LSPS2Request::GetInfo(_)) => {
                        let response = LSPS2Response::GetInfo(GetInfoResponse {
                            opening_fee_params_menu: opening_fee_params_menu
                                .into_iter()
                                .map(|param| {
                                    param.into_opening_fee_params(&self.config.promise_secret)
                                })
                                .collect(),
                        });
                        self.enqueue_response(counterparty_node_id, request_id, response);
                        Ok(())
                    }
                    _ => Err(APIError::APIMisuseError {
                        err: format!(
                            "No pending get_info request for request_id: {:?}",
                            request_id
                        ),
                    }),
                }
            }
            None => Err(APIError::APIMisuseError {
                err: format!("No state for the counterparty exists: {:?}", counterparty_node_id),
            }),
        }
    }

    /// Used by LSP to provide client with the intercept scid and cltv_expiry_delta to use in their invoice.
    ///
    /// Should be called in response to receiving a [`LSPS2ServiceEvent::BuyRequest`] event.
    ///
    /// [`LSPS2ServiceEvent::BuyRequest`]: crate::lsps2::event::LSPS2ServiceEvent::BuyRequest
    pub fn invoice_parameters_generated(
        &self, counterparty_node_id: &PublicKey, request_id: RequestId, intercept_scid: u64,
        cltv_expiry_delta: u32, client_trusts_lsp: bool, user_channel_id: u128,
    ) -> Result<(), APIError> {
        let outer_state_lock = self.per_peer_state.read().unwrap();

        match outer_state_lock.get(counterparty_node_id) {
            Some(inner_state_lock) => {
                let mut peer_state = inner_state_lock.lock().unwrap();

                match peer_state.pending_requests.remove(&request_id) {
                    Some(LSPS2Request::Buy(buy_request)) => {
                        {
                            let mut peer_by_intercept_scid =
                                self.peer_by_intercept_scid.write().unwrap();
                            peer_by_intercept_scid.insert(intercept_scid, *counterparty_node_id);
                        }

                        let outbound_jit_channel = OutboundJITChannel::new(
                            buy_request.payment_size_msat,
                            buy_request.opening_fee_params,
                            user_channel_id,
                        );

                        peer_state
                            .intercept_scid_by_user_channel_id
                            .insert(user_channel_id, intercept_scid);
                        peer_state.insert_outbound_channel(intercept_scid, outbound_jit_channel);

                        self.enqueue_response(
                            counterparty_node_id,
                            request_id,
                            LSPS2Response::Buy(BuyResponse {
                                intercept_scid: intercept_scid.into(),
                                lsp_cltv_expiry_delta: cltv_expiry_delta,
                                client_trusts_lsp,
                            }),
                        );

                        Ok(())
                    }
                    _ => Err(APIError::APIMisuseError {
                        err: format!("No pending buy request for request_id: {:?}", request_id),
                    }),
                }
            }
            None => Err(APIError::APIMisuseError {
                err: format!("No state for the counterparty exists: {:?}", counterparty_node_id),
            }),
        }
    }

    /// Forward [`Event::HTLCIntercepted`] event parameters into this function.
    ///
    /// Will fail the intercepted HTLC if the intercept scid matches a payment we are expecting
    /// but the payment amount is incorrect or the expiry has passed.
    ///
    /// Will generate a [`LSPS2ServiceEvent::OpenChannel`] event if the intercept scid matches a payment we are expected
    /// and the payment amount is correct and the offer has not expired.
    ///
    /// Will do nothing if the intercept scid does not match any of the ones we gave out.
    ///
    /// [`Event::HTLCIntercepted`]: lightning::events::Event::HTLCIntercepted
    /// [`LSPS2ServiceEvent::OpenChannel`]: crate::lsps2::event::LSPS2ServiceEvent::OpenChannel
    pub fn htlc_intercepted(
        &self, intercept_scid: u64, intercept_id: InterceptId, expected_outbound_amount_msat: u64,
    ) -> Result<(), APIError> {
        let peer_by_intercept_scid = self.peer_by_intercept_scid.read().unwrap();
        if let Some(counterparty_node_id) = peer_by_intercept_scid.get(&intercept_scid) {
            let outer_state_lock = self.per_peer_state.read().unwrap();
            match outer_state_lock.get(counterparty_node_id) {
                Some(inner_state_lock) => {
                    let mut peer_state = inner_state_lock.lock().unwrap();
                    if let Some(jit_channel) =
                        peer_state.outbound_channels_by_intercept_scid.get_mut(&intercept_scid)
                    {
                        let htlc = InterceptedHTLC { intercept_id, expected_outbound_amount_msat };
                        match jit_channel.htlc_intercepted(htlc) {
                            Ok(Some((opening_fee_msat, amt_to_forward_msat))) => {
                                self.enqueue_event(Event::LSPS2Service(
                                    LSPS2ServiceEvent::OpenChannel {
                                        their_network_key: counterparty_node_id.clone(),
                                        amt_to_forward_msat,
                                        opening_fee_msat,
                                        user_channel_id: jit_channel.user_channel_id,
                                        intercept_scid,
                                    },
                                ));
                            }
                            Ok(None) => {}
                            Err(e) => {
                                self.channel_manager
                                    .get_cm()
                                    .fail_intercepted_htlc(intercept_id)?;
                                peer_state
                                    .outbound_channels_by_intercept_scid
                                    .remove(&intercept_scid);
                                // TODO: cleanup peer_by_intercept_scid
                                return Err(APIError::APIMisuseError { err: e.err });
                            }
                        }
                    }
                }
                None => {
                    return Err(APIError::APIMisuseError {
                        err: format!("No counterparty found for scid: {}", intercept_scid),
                    });
                }
            }
        }

        Ok(())
    }

    /// Forward [`Event::ChannelReady`] event parameters into this function.
    ///
    /// Will forward the intercepted HTLC if it matches a channel
    /// we need to forward a payment over otherwise it will be ignored.
    ///
    /// [`Event::ChannelReady`]: lightning::events::Event::ChannelReady
    pub fn channel_ready(
        &self, user_channel_id: u128, channel_id: &ChannelId, counterparty_node_id: &PublicKey,
    ) -> Result<(), APIError> {
        let outer_state_lock = self.per_peer_state.read().unwrap();
        match outer_state_lock.get(counterparty_node_id) {
            Some(inner_state_lock) => {
                let mut peer_state = inner_state_lock.lock().unwrap();
                if let Some(intercept_scid) =
                    peer_state.intercept_scid_by_user_channel_id.get(&user_channel_id).copied()
                {
                    if let Some(jit_channel) =
                        peer_state.outbound_channels_by_intercept_scid.get_mut(&intercept_scid)
                    {
                        match jit_channel.channel_ready() {
                            Ok((htlcs, total_amt_to_forward_msat)) => {
                                let amounts_to_forward_msat = calculate_amount_to_forward_per_htlc(
                                    &htlcs,
                                    total_amt_to_forward_msat,
                                );

                                for (intercept_id, amount_to_forward_msat) in
                                    amounts_to_forward_msat
                                {
                                    self.channel_manager.get_cm().forward_intercepted_htlc(
                                        intercept_id,
                                        channel_id,
                                        *counterparty_node_id,
                                        amount_to_forward_msat,
                                    )?;
                                }
                            }
                            Err(e) => {
                                return Err(APIError::APIMisuseError {
                                    err: format!(
                                        "Failed to transition to channel ready: {}",
                                        e.err
                                    ),
                                })
                            }
                        }
                    } else {
                        return Err(APIError::APIMisuseError {
                            err: format!(
                                "Could not find a channel with user_channel_id {}",
                                user_channel_id
                            ),
                        });
                    }
                } else {
                    return Err(APIError::APIMisuseError {
                        err: format!(
                            "Could not find a channel with that user_channel_id {}",
                            user_channel_id
                        ),
                    });
                }
            }
            None => {
                return Err(APIError::APIMisuseError {
                    err: format!("No counterparty state for: {}", counterparty_node_id),
                });
            }
        }

        Ok(())
    }

    fn enqueue_response(
        &self, counterparty_node_id: &PublicKey, request_id: RequestId, response: LSPS2Response,
    ) {
        self.pending_messages
            .enqueue(counterparty_node_id, LSPS2Message::Response(request_id, response).into());
    }

    fn enqueue_event(&self, event: Event) {
        self.pending_events.enqueue(event);
    }

    fn handle_get_info_request(
        &self, request_id: RequestId, counterparty_node_id: &PublicKey, params: GetInfoRequest,
    ) -> Result<(), LightningError> {
        let mut outer_state_lock = self.per_peer_state.write().unwrap();
        let inner_state_lock: &mut Mutex<PeerState> =
            outer_state_lock.entry(*counterparty_node_id).or_insert(Mutex::new(PeerState::new()));
        let mut peer_state_lock = inner_state_lock.lock().unwrap();
        peer_state_lock
            .pending_requests
            .insert(request_id.clone(), LSPS2Request::GetInfo(params.clone()));

        self.enqueue_event(Event::LSPS2Service(LSPS2ServiceEvent::GetInfo {
            request_id,
            counterparty_node_id: *counterparty_node_id,
            token: params.token,
        }));
        Ok(())
    }

    fn handle_buy_request(
        &self, request_id: RequestId, counterparty_node_id: &PublicKey, params: BuyRequest,
    ) -> Result<(), LightningError> {
        if let Some(payment_size_msat) = params.payment_size_msat {
            if payment_size_msat < params.opening_fee_params.min_payment_size_msat {
                self.enqueue_response(
                    counterparty_node_id,
                    request_id,
                    LSPS2Response::BuyError(ResponseError {
                        code: LSPS2_BUY_REQUEST_PAYMENT_SIZE_TOO_SMALL_ERROR_CODE,
                        message: "payment size is below our minimum supported payment size"
                            .to_string(),
                        data: None,
                    }),
                );
                return Err(LightningError {
                    err: "payment size is below our minimum supported payment size".to_string(),
                    action: ErrorAction::IgnoreAndLog(Level::Info),
                });
            }

            if payment_size_msat > params.opening_fee_params.max_payment_size_msat {
                self.enqueue_response(
                    counterparty_node_id,
                    request_id,
                    LSPS2Response::BuyError(ResponseError {
                        code: LSPS2_BUY_REQUEST_PAYMENT_SIZE_TOO_LARGE_ERROR_CODE,
                        message: "payment size is above our maximum supported payment size"
                            .to_string(),
                        data: None,
                    }),
                );
                return Err(LightningError {
                    err: "payment size is above our maximum supported payment size".to_string(),
                    action: ErrorAction::IgnoreAndLog(Level::Info),
                });
            }

            match compute_opening_fee(
                payment_size_msat,
                params.opening_fee_params.min_fee_msat,
                params.opening_fee_params.proportional.into(),
            ) {
                Some(opening_fee) => {
                    if opening_fee >= payment_size_msat {
                        self.enqueue_response(
                            counterparty_node_id,
                            request_id,
                            LSPS2Response::BuyError(ResponseError {
                                code: LSPS2_BUY_REQUEST_PAYMENT_SIZE_TOO_SMALL_ERROR_CODE,
                                message: "payment size is too small to cover the opening fee"
                                    .to_string(),
                                data: None,
                            }),
                        );
                        return Err(LightningError {
                            err: "payment size is too small to cover the opening fee".to_string(),
                            action: ErrorAction::IgnoreAndLog(Level::Info),
                        });
                    }
                }
                None => {
                    self.enqueue_response(
                        counterparty_node_id,
                        request_id,
                        LSPS2Response::BuyError(ResponseError {
                            code: LSPS2_BUY_REQUEST_PAYMENT_SIZE_TOO_LARGE_ERROR_CODE,
                            message: "overflow error when calculating opening_fee".to_string(),
                            data: None,
                        }),
                    );
                    return Err(LightningError {
                        err: "overflow error when calculating opening_fee".to_string(),
                        action: ErrorAction::IgnoreAndLog(Level::Info),
                    });
                }
            }
        }

        // TODO: if payment_size_msat is specified, make sure our node has sufficient incoming liquidity from public network to receive it.

        if !is_valid_opening_fee_params(&params.opening_fee_params, &self.config.promise_secret) {
            self.enqueue_response(
                counterparty_node_id,
                request_id,
                LSPS2Response::BuyError(ResponseError {
                    code: LSPS2_BUY_REQUEST_INVALID_OPENING_FEE_PARAMS_ERROR_CODE,
                    message: "valid_until is already past OR the promise did not match the provided parameters".to_string(),
                    data: None,
                }),
            );
            return Err(LightningError {
                err: "invalid opening fee parameters were supplied by client".to_string(),
                action: ErrorAction::IgnoreAndLog(Level::Info),
            });
        }

        let mut outer_state_lock = self.per_peer_state.write().unwrap();
        let inner_state_lock =
            outer_state_lock.entry(*counterparty_node_id).or_insert(Mutex::new(PeerState::new()));
        let mut peer_state_lock = inner_state_lock.lock().unwrap();
        peer_state_lock
            .pending_requests
            .insert(request_id.clone(), LSPS2Request::Buy(params.clone()));

        self.enqueue_event(Event::LSPS2Service(LSPS2ServiceEvent::BuyRequest {
            request_id,
            counterparty_node_id: *counterparty_node_id,
            opening_fee_params: params.opening_fee_params,
            payment_size_msat: params.payment_size_msat,
        }));

        Ok(())
    }
}

impl<CM: Deref + Clone> ProtocolMessageHandler for LSPS2ServiceHandler<CM>
where
    CM::Target: AChannelManager,
{
    type ProtocolMessage = LSPS2Message;
    const PROTOCOL_NUMBER: Option<u16> = Some(2);

    fn handle_message(
        &self, message: Self::ProtocolMessage, counterparty_node_id: &PublicKey,
    ) -> Result<(), LightningError> {
        match message {
            LSPS2Message::Request(request_id, request) => match request {
                LSPS2Request::GetInfo(params) => {
                    self.handle_get_info_request(request_id, counterparty_node_id, params)
                }
                LSPS2Request::Buy(params) => {
                    self.handle_buy_request(request_id, counterparty_node_id, params)
                }
            },
            _ => {
                debug_assert!(
                    false,
                    "Service handler received LSPS2 response message. This should never happen."
                );
                Err(LightningError { err: format!("Service handler received LSPS2 response message from node {:?}. This should never happen.", counterparty_node_id), action: ErrorAction::IgnoreAndLog(Level::Info)})
            }
        }
    }
}

fn calculate_amount_to_forward_per_htlc(
    htlcs: &[InterceptedHTLC], total_amt_to_forward_msat: u64,
) -> Vec<(InterceptId, u64)> {
    // TODO: we should eventually make sure the HTLCs are all above ChannelDetails::next_outbound_minimum_msat
    let total_received_msat: u64 =
        htlcs.iter().map(|htlc| htlc.expected_outbound_amount_msat).sum();

    match total_received_msat.checked_sub(total_amt_to_forward_msat) {
        Some(total_fee_msat) => {
            let mut fee_remaining_msat = total_fee_msat;

            let mut per_htlc_forwards = vec![];

            for (index, htlc) in htlcs.iter().enumerate() {
                let proportional_fee_amt_msat =
                    total_fee_msat * (htlc.expected_outbound_amount_msat / total_received_msat);

                let mut actual_fee_amt_msat =
                    core::cmp::min(fee_remaining_msat, proportional_fee_amt_msat);
                fee_remaining_msat -= actual_fee_amt_msat;

                if index == htlcs.len() - 1 {
                    actual_fee_amt_msat += fee_remaining_msat;
                }

                let amount_to_forward_msat =
                    htlc.expected_outbound_amount_msat.saturating_sub(actual_fee_amt_msat);

                per_htlc_forwards.push((htlc.intercept_id, amount_to_forward_msat))
            }

            per_htlc_forwards
        }
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use proptest::prelude::*;

    const MAX_VALUE_MSAT: u64 = 21_000_000_0000_0000_000;

    fn arb_forward_amounts() -> impl Strategy<Value = (u64, u64, u64, u64)> {
        (1u64..MAX_VALUE_MSAT, 1u64..MAX_VALUE_MSAT, 1u64..MAX_VALUE_MSAT, 1u64..MAX_VALUE_MSAT)
            .prop_map(|(a, b, c, d)| {
                (a, b, c, core::cmp::min(d, a.saturating_add(b).saturating_add(c)))
            })
    }

    proptest! {
        #[test]
        fn test_calculate_amount_to_forward((o_0, o_1, o_2, total_amt_to_forward_msat) in arb_forward_amounts()) {
            let htlcs = vec![
                InterceptedHTLC {
                    intercept_id: InterceptId([0; 32]),
                    expected_outbound_amount_msat: o_0
                },
                InterceptedHTLC {
                    intercept_id: InterceptId([1; 32]),
                    expected_outbound_amount_msat: o_1
                },
                InterceptedHTLC {
                    intercept_id: InterceptId([2; 32]),
                    expected_outbound_amount_msat: o_2
                },
            ];

            let result = calculate_amount_to_forward_per_htlc(&htlcs, total_amt_to_forward_msat);
            let total_received_msat = o_0 + o_1 + o_2;

            if total_received_msat < total_amt_to_forward_msat {
                assert_eq!(result.len(), 0);
            } else {
                assert_ne!(result.len(), 0);
                assert_eq!(result[0].0, htlcs[0].intercept_id);
                assert_eq!(result[1].0, htlcs[1].intercept_id);
                assert_eq!(result[2].0, htlcs[2].intercept_id);
                assert!(result[0].1 <= o_0);
                assert!(result[1].1 <= o_1);
                assert!(result[2].1 <= o_2);

                let result_sum = result.iter().map(|(_, f)| f).sum::<u64>();
                assert!(result_sum >= total_amt_to_forward_msat);
                let five_pct = result_sum as f32 * 0.1;
                let fair_share_0 = ((o_0 as f32 / total_received_msat as f32) * result_sum as f32).max(o_0 as f32);
                assert!(result[0].1 as f32 <= fair_share_0 + five_pct);
                let fair_share_1 = ((o_1 as f32 / total_received_msat as f32) * result_sum as f32).max(o_1 as f32);
                assert!(result[1].1 as f32 <= fair_share_1 + five_pct);
                let fair_share_2 = ((o_2 as f32 / total_received_msat as f32) * result_sum as f32).max(o_2 as f32);
                assert!(result[2].1 as f32 <= fair_share_2 + five_pct);
            }

        }
    }
}
