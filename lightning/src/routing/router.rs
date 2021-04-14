// This file is Copyright its original authors, visible in version control
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

//! The top-level routing/network map tracking logic lives here.
//!
//! You probably want to create a NetGraphMsgHandler and use that as your RoutingMessageHandler and then
//! interrogate it to get routes for your own payments.

use bitcoin::secp256k1::key::PublicKey;

use ln::channelmanager::ChannelDetails;
use ln::features::{ChannelFeatures, InvoiceFeatures, NodeFeatures};
use ln::msgs::{DecodeError, ErrorAction, LightningError, MAX_VALUE_MSAT};
use routing::network_graph::{NetworkGraph, RoutingFees};
use util::ser::{Writeable, Readable};
use util::logger::Logger;

use std::cmp;
use std::collections::{HashMap, BinaryHeap};
use std::ops::Deref;

/// A hop in a route
#[derive(Clone, PartialEq)]
pub struct RouteHop {
	/// The node_id of the node at this hop.
	pub pubkey: PublicKey,
	/// The node_announcement features of the node at this hop. For the last hop, these may be
	/// amended to match the features present in the invoice this node generated.
	pub node_features: NodeFeatures,
	/// The channel that should be used from the previous hop to reach this node.
	pub short_channel_id: u64,
	/// The channel_announcement features of the channel that should be used from the previous hop
	/// to reach this node.
	pub channel_features: ChannelFeatures,
	/// The fee taken on this hop (for paying for the use of the *next* channel in the path).
	/// For the last hop, this should be the full value of the payment (might be more than
	/// requested if we had to match htlc_minimum_msat).
	pub fee_msat: u64,
	/// The CLTV delta added for this hop. For the last hop, this should be the full CLTV value
	/// expected at the destination, in excess of the current block height.
	pub cltv_expiry_delta: u32,
}

/// (C-not exported)
impl Writeable for Vec<RouteHop> {
	fn write<W: ::util::ser::Writer>(&self, writer: &mut W) -> Result<(), ::std::io::Error> {
		(self.len() as u8).write(writer)?;
		for hop in self.iter() {
			hop.pubkey.write(writer)?;
			hop.node_features.write(writer)?;
			hop.short_channel_id.write(writer)?;
			hop.channel_features.write(writer)?;
			hop.fee_msat.write(writer)?;
			hop.cltv_expiry_delta.write(writer)?;
		}
		Ok(())
	}
}

/// (C-not exported)
impl Readable for Vec<RouteHop> {
	fn read<R: ::std::io::Read>(reader: &mut R) -> Result<Vec<RouteHop>, DecodeError> {
		let hops_count: u8 = Readable::read(reader)?;
		let mut hops = Vec::with_capacity(hops_count as usize);
		for _ in 0..hops_count {
			hops.push(RouteHop {
				pubkey: Readable::read(reader)?,
				node_features: Readable::read(reader)?,
				short_channel_id: Readable::read(reader)?,
				channel_features: Readable::read(reader)?,
				fee_msat: Readable::read(reader)?,
				cltv_expiry_delta: Readable::read(reader)?,
			});
		}
		Ok(hops)
	}
}

/// A route directs a payment from the sender (us) to the recipient. If the recipient supports MPP,
/// it can take multiple paths. Each path is composed of one or more hops through the network.
#[derive(Clone, PartialEq)]
pub struct Route {
	/// The list of routes taken for a single (potentially-)multi-part payment. The pubkey of the
	/// last RouteHop in each path must be the same.
	/// Each entry represents a list of hops, NOT INCLUDING our own, where the last hop is the
	/// destination. Thus, this must always be at least length one. While the maximum length of any
	/// given path is variable, keeping the length of any path to less than 20 should currently
	/// ensure it is viable.
	pub paths: Vec<Vec<RouteHop>>,
}

impl Writeable for Route {
	fn write<W: ::util::ser::Writer>(&self, writer: &mut W) -> Result<(), ::std::io::Error> {
		(self.paths.len() as u64).write(writer)?;
		for hops in self.paths.iter() {
			hops.write(writer)?;
		}
		Ok(())
	}
}

impl Readable for Route {
	fn read<R: ::std::io::Read>(reader: &mut R) -> Result<Route, DecodeError> {
		let path_count: u64 = Readable::read(reader)?;
		let mut paths = Vec::with_capacity(cmp::min(path_count, 128) as usize);
		for _ in 0..path_count {
			paths.push(Readable::read(reader)?);
		}
		Ok(Route { paths })
	}
}

/// A channel descriptor which provides a last-hop route to get_route
#[derive(Clone)]
pub struct RouteHint {
	/// The node_id of the non-target end of the route
	pub src_node_id: PublicKey,
	/// The short_channel_id of this channel
	pub short_channel_id: u64,
	/// The fees which must be paid to use this channel
	pub fees: RoutingFees,
	/// The difference in CLTV values between this node and the next node.
	pub cltv_expiry_delta: u16,
	/// The minimum value, in msat, which must be relayed to the next hop.
	pub htlc_minimum_msat: Option<u64>,
	/// The maximum value in msat available for routing with a single HTLC.
	pub htlc_maximum_msat: Option<u64>,
}

#[derive(Eq, PartialEq)]
struct RouteGraphNode {
	pubkey: PublicKey,
	lowest_fee_to_peer_through_node: u64,
	lowest_fee_to_node: u64,
	// The maximum value a yet-to-be-constructed payment path might flow through this node.
	// This value is upper-bounded by us by:
	// - how much is needed for a path being constructed
	// - how much value can channels following this node (up to the destination) can contribute,
	//   considering their capacity and fees
	value_contribution_msat: u64
}

impl cmp::Ord for RouteGraphNode {
	fn cmp(&self, other: &RouteGraphNode) -> cmp::Ordering {
		other.lowest_fee_to_peer_through_node.cmp(&self.lowest_fee_to_peer_through_node)
			.then_with(|| other.pubkey.serialize().cmp(&self.pubkey.serialize()))
	}
}

impl cmp::PartialOrd for RouteGraphNode {
	fn partial_cmp(&self, other: &RouteGraphNode) -> Option<cmp::Ordering> {
		Some(self.cmp(other))
	}
}

struct DummyDirectionalChannelInfo {
	cltv_expiry_delta: u32,
	htlc_minimum_msat: u64,
	htlc_maximum_msat: Option<u64>,
	fees: RoutingFees,
}

/// It's useful to keep track of the hops associated with the fees required to use them,
/// so that we can choose cheaper paths (as per Dijkstra's algorithm).
/// Fee values should be updated only in the context of the whole path, see update_value_and_recompute_fees.
/// These fee values are useful to choose hops as we traverse the graph "payee-to-payer".
#[derive(Clone)]
struct PathBuildingHop {
	/// Hop-specific details unrelated to the path during the routing phase,
	/// but rather relevant to the LN graph.
	route_hop: RouteHop,
	/// Minimal fees required to route to the source node of the current hop via any of its inbound channels.
	src_lowest_inbound_fees: RoutingFees,
	/// Fees of the channel used in this hop.
	channel_fees: RoutingFees,
	/// All the fees paid *after* this channel on the way to the destination
	next_hops_fee_msat: u64,
	/// Fee paid for the use of the current channel (see channel_fees).
	/// The value will be actually deducted from the counterparty balance on the previous link.
	hop_use_fee_msat: u64,
	/// Used to compare channels when choosing the for routing.
	/// Includes paying for the use of a hop and the following hops, as well as
	/// an estimated cost of reaching this hop.
	/// Might get stale when fees are recomputed. Primarily for internal use.
	total_fee_msat: u64,
	/// This is useful for update_value_and_recompute_fees to make sure
	/// we don't fall below the minimum. Should not be updated manually and
	/// generally should not be accessed.
	htlc_minimum_msat: u64,
}

// Instantiated with a list of hops with correct data in them collected during path finding,
// an instance of this struct should be further modified only via given methods.
#[derive(Clone)]
struct PaymentPath {
	hops: Vec<PathBuildingHop>,
}

impl PaymentPath {

	// TODO: Add a value_msat field to PaymentPath and use it instead of this function.
	fn get_value_msat(&self) -> u64 {
		self.hops.last().unwrap().route_hop.fee_msat
	}

	fn get_total_fee_paid_msat(&self) -> u64 {
		if self.hops.len() < 1 {
			return 0;
		}
		let mut result = 0;
		// Can't use next_hops_fee_msat because it gets outdated.
		for (i, hop) in self.hops.iter().enumerate() {
			if i != self.hops.len() - 1 {
				result += hop.route_hop.fee_msat;
			}
		}
		return result;
	}

	// If the amount transferred by the path is updated, the fees should be adjusted. Any other way
	// to change fees may result in an inconsistency.
	//
	// Sometimes we call this function right after constructing a path which has inconsistent
	// (in terms of reaching htlc_minimum_msat), so that this function puts the fees in order.
	// In that case we call it on the "same" amount we initially allocated for this path, and which
	// could have been reduced on the way. In that case, there is also a risk of exceeding
	// available_liquidity inside this function, because the function is unaware of this bound.
	// In our specific recomputation cases where we never increase the value the risk is pretty low.
	// This function, however, does not support arbitrarily increasing the value being transferred,
	// and the exception will be triggered.
	fn update_value_and_recompute_fees(&mut self, value_msat: u64) {
		assert!(value_msat <= self.hops.last().unwrap().route_hop.fee_msat);

		let mut total_fee_paid_msat = 0 as u64;
		for i in (0..self.hops.len()).rev() {
			let last_hop = i == self.hops.len() - 1;

			// For non-last-hop, this value will represent the fees paid on the current hop. It
			// will consist of the fees for the use of the next hop, and extra fees to match
			// htlc_minimum_msat of the current channel. Last hop is handled separately.
			let mut cur_hop_fees_msat = 0;
			if !last_hop {
				cur_hop_fees_msat = self.hops.get(i + 1).unwrap().hop_use_fee_msat;
			}

			let mut cur_hop = self.hops.get_mut(i).unwrap();
			cur_hop.next_hops_fee_msat = total_fee_paid_msat;
			// Overpay in fees if we can't save these funds due to htlc_minimum_msat.
			// We try to account for htlc_minimum_msat in scoring (add_entry!), so that nodes don't
			// set it too high just to maliciously take more fees by exploiting this
			// match htlc_minimum_msat logic.
			let mut cur_hop_transferred_amount_msat = total_fee_paid_msat + value_msat;
			if let Some(extra_fees_msat) = cur_hop.htlc_minimum_msat.checked_sub(cur_hop_transferred_amount_msat) {
				// Note that there is a risk that *previous hops* (those closer to us, as we go
				// payee->our_node here) would exceed their htlc_maximum_msat or available balance.
				//
				// This might make us end up with a broken route, although this should be super-rare
				// in practice, both because of how healthy channels look like, and how we pick
				// channels in add_entry.
				// Also, this can't be exploited more heavily than *announce a free path and fail
				// all payments*.
				cur_hop_transferred_amount_msat += extra_fees_msat;
				total_fee_paid_msat += extra_fees_msat;
				cur_hop_fees_msat += extra_fees_msat;
			}

			if last_hop {
				// Final hop is a special case: it usually has just value_msat (by design), but also
				// it still could overpay for the htlc_minimum_msat.
				cur_hop.route_hop.fee_msat = cur_hop_transferred_amount_msat;
			} else {
				// Propagate updated fees for the use of the channels to one hop back, where they
				// will be actually paid (fee_msat). The last hop is handled above separately.
				cur_hop.route_hop.fee_msat = cur_hop_fees_msat;
			}

			// Fee for the use of the current hop which will be deducted on the previous hop.
			// Irrelevant for the first hop, as it doesn't have the previous hop, and the use of
			// this channel is free for us.
			if i != 0 {
				if let Some(new_fee) = compute_fees(cur_hop_transferred_amount_msat, cur_hop.channel_fees) {
					cur_hop.hop_use_fee_msat = new_fee;
					total_fee_paid_msat += new_fee;
				} else {
					// It should not be possible because this function is called only to reduce the
					// value. In that case, compute_fee was already called with the same fees for
					// larger amount and there was no overflow.
					unreachable!();
				}
			}
		}
	}
}

fn compute_fees(amount_msat: u64, channel_fees: RoutingFees) -> Option<u64> {
	let proportional_fee_millions =
		amount_msat.checked_mul(channel_fees.proportional_millionths as u64);
	if let Some(new_fee) = proportional_fee_millions.and_then(|part| {
			(channel_fees.base_msat as u64).checked_add(part / 1_000_000) }) {

		Some(new_fee)
	} else {
		// This function may be (indirectly) called without any verification,
		// with channel_fees provided by a caller. We should handle it gracefully.
		None
	}
}

/// Gets a route from us (payer) to the given target node (payee).
///
/// If the payee provided features in their invoice, they should be provided via payee_features.
/// Without this, MPP will only be used if the payee's features are available in the network graph.
///
/// Extra routing hops between known nodes and the target will be used if they are included in
/// last_hops.
///
/// If some channels aren't announced, it may be useful to fill in a first_hops with the
/// results from a local ChannelManager::list_usable_channels() call. If it is filled in, our
/// view of our local channels (from net_graph_msg_handler) will be ignored, and only those
/// in first_hops will be used.
///
/// Panics if first_hops contains channels without short_channel_ids
/// (ChannelManager::list_usable_channels will never include such channels).
///
/// The fees on channels from us to next-hops are ignored (as they are assumed to all be
/// equal), however the enabled/disabled bit on such channels as well as the
/// htlc_minimum_msat/htlc_maximum_msat *are* checked as they may change based on the receiving node.
pub fn get_route<L: Deref>(our_node_id: &PublicKey, network: &NetworkGraph, payee: &PublicKey, payee_features: Option<InvoiceFeatures>, first_hops: Option<&[&ChannelDetails]>,
	last_hops: &[&RouteHint], final_value_msat: u64, final_cltv: u32, logger: L) -> Result<Route, LightningError> where L::Target: Logger {
	// TODO: Obviously *only* using total fee cost sucks. We should consider weighting by
	// uptime/success in using a node in the past.
	if *payee == *our_node_id {
		return Err(LightningError{err: "Cannot generate a route to ourselves".to_owned(), action: ErrorAction::IgnoreError});
	}

	if final_value_msat > MAX_VALUE_MSAT {
		return Err(LightningError{err: "Cannot generate a route of more value than all existing satoshis".to_owned(), action: ErrorAction::IgnoreError});
	}

	if final_value_msat == 0 {
		return Err(LightningError{err: "Cannot send a payment of 0 msat".to_owned(), action: ErrorAction::IgnoreError});
	}

	for last_hop in last_hops {
		if last_hop.src_node_id == *payee {
			return Err(LightningError{err: "Last hop cannot have a payee as a source.".to_owned(), action: ErrorAction::IgnoreError});
		}
	}

	// The general routing idea is the following:
	// 1. Fill first/last hops communicated by the caller.
	// 2. Attempt to construct a path from payer to payee for transferring
	//    any ~sufficient (described later) value.
	//    If succeed, remember which channels were used and how much liquidity they have available,
	//    so that future paths don't rely on the same liquidity.
	// 3. Prooceed to the next step if:
	//    - we hit the recommended target value;
	//    - OR if we could not construct a new path. Any next attempt will fail too.
	//    Otherwise, repeat step 2.
	// 4. See if we managed to collect paths which aggregately are able to transfer target value
	//    (not recommended value). If yes, proceed. If not, fail routing.
	// 5. Randomly combine paths into routes having enough to fulfill the payment. (TODO: knapsack)
	// 6. Of all the found paths, select only those with the lowest total fee.
	// 7. The last path in every selected route is likely to be more than we need.
	//    Reduce its value-to-transfer and recompute fees.
	// 8. Choose the best route by the lowest total fee.

	// As for the actual search algorithm,
	// we do a payee-to-payer Dijkstra's sorting by each node's distance from the payee
	// plus the minimum per-HTLC fee to get from it to another node (aka "shitty A*").
	// TODO: There are a few tweaks we could do, including possibly pre-calculating more stuff
	// to use as the A* heuristic beyond just the cost to get one node further than the current
	// one.

	let dummy_directional_info = DummyDirectionalChannelInfo { // used for first_hops routes
		cltv_expiry_delta: 0,
		htlc_minimum_msat: 0,
		htlc_maximum_msat: None,
		fees: RoutingFees {
			base_msat: 0,
			proportional_millionths: 0,
		}
	};

	let mut targets = BinaryHeap::new(); //TODO: Do we care about switching to eg Fibbonaci heap?
	let mut dist = HashMap::with_capacity(network.get_nodes().len());

	// When arranging a route, we select multiple paths so that we can make a multi-path payment.
	// Don't stop searching for paths when we think they're
	// sufficient to transfer a given value aggregately.
	// Search for higher value, so that we collect many more paths,
	// and then select the best combination among them.
	const ROUTE_CAPACITY_PROVISION_FACTOR: u64 = 3;
	let recommended_value_msat = final_value_msat * ROUTE_CAPACITY_PROVISION_FACTOR as u64;

	// Allow MPP only if we have a features set from somewhere that indicates the payee supports
	// it. If the payee supports it they're supposed to include it in the invoice, so that should
	// work reliably.
	let allow_mpp = if let Some(features) = &payee_features {
		features.supports_basic_mpp()
	} else if let Some(node) = network.get_nodes().get(&payee) {
		if let Some(node_info) = node.announcement_info.as_ref() {
			node_info.features.supports_basic_mpp()
		} else { false }
	} else { false };

	// Step (1).
	// Prepare the data we'll use for payee-to-payer search by
	// inserting first hops suggested by the caller as targets.
	// Our search will then attempt to reach them while traversing from the payee node.
	let mut first_hop_targets = HashMap::with_capacity(if first_hops.is_some() { first_hops.as_ref().unwrap().len() } else { 0 });
	if let Some(hops) = first_hops {
		for chan in hops {
			let short_channel_id = chan.short_channel_id.expect("first_hops should be filled in with usable channels, not pending ones");
			if chan.remote_network_id == *our_node_id {
				return Err(LightningError{err: "First hop cannot have our_node_id as a destination.".to_owned(), action: ErrorAction::IgnoreError});
			}
			first_hop_targets.insert(chan.remote_network_id, (short_channel_id, chan.counterparty_features.clone(), chan.outbound_capacity_msat));
		}
		if first_hop_targets.is_empty() {
			return Err(LightningError{err: "Cannot route when there are no outbound routes away from us".to_owned(), action: ErrorAction::IgnoreError});
		}
	}

	// We don't want multiple paths (as per MPP) share liquidity of the same channels.
	// This map allows paths to be aware of the channel use by other paths in the same call.
	// This would help to make a better path finding decisions and not "overbook" channels.
	// It is unaware of the directions (except for `outbound_capacity_msat` in `first_hops`).
	let mut bookkeeped_channels_liquidity_available_msat = HashMap::new();

	// Keeping track of how much value we already collected across other paths. Helps to decide:
	// - how much a new path should be transferring (upper bound);
	// - whether a channel should be disregarded because
	//   it's available liquidity is too small comparing to how much more we need to collect;
	// - when we want to stop looking for new paths.
	let mut already_collected_value_msat = 0;

	macro_rules! add_entry {
		// Adds entry which goes from $src_node_id to $dest_node_id
		// over the channel with id $chan_id with fees described in
		// $directional_info.
		// $next_hops_fee_msat represents the fees paid for using all the channel *after* this one,
		// since that value has to be transferred over this channel.
		( $chan_id: expr, $src_node_id: expr, $dest_node_id: expr, $directional_info: expr, $capacity_sats: expr, $chan_features: expr, $next_hops_fee_msat: expr,
		   $next_hops_value_contribution: expr ) => {
			// Channels to self should not be used. This is more of belt-and-suspenders, because in
			// practice these cases should be caught earlier:
			// - for regular channels at channel announcement (TODO)
			// - for first and last hops early in get_route
			if $src_node_id != $dest_node_id.clone() {
				let available_liquidity_msat = bookkeeped_channels_liquidity_available_msat.entry($chan_id.clone()).or_insert_with(|| {
					let mut initial_liquidity_available_msat = None;
					if let Some(capacity_sats) = $capacity_sats {
						initial_liquidity_available_msat = Some(capacity_sats * 1000);
					}

					if let Some(htlc_maximum_msat) = $directional_info.htlc_maximum_msat {
						if let Some(available_msat) = initial_liquidity_available_msat {
							initial_liquidity_available_msat = Some(cmp::min(available_msat, htlc_maximum_msat));
						} else {
							initial_liquidity_available_msat = Some(htlc_maximum_msat);
						}
					}

					match initial_liquidity_available_msat {
						Some(available_msat) => available_msat,
						// We assume channels with unknown balance have
						// a capacity of 0.0025 BTC (or 250_000 sats).
						None => 250_000 * 1000
					}
				});

				// It is tricky to substract $next_hops_fee_msat from available liquidity here.
				// It may be misleading because we might later choose to reduce the value transferred
				// over these channels, and the channel which was insufficient might become sufficient.
				// Worst case: we drop a good channel here because it can't cover the high following
				// fees caused by one expensive channel, but then this channel could have been used
				// if the amount being transferred over this path is lower.
				// We do this for now, but this is a subject for removal.
				if let Some(available_value_contribution_msat) = available_liquidity_msat.checked_sub($next_hops_fee_msat) {

					// Routing Fragmentation Mitigation heuristic:
					//
					// Routing fragmentation across many payment paths increases the overall routing
					// fees as you have irreducible routing fees per-link used (`fee_base_msat`).
					// Taking too many smaller paths also increases the chance of payment failure.
					// Thus to avoid this effect, we require from our collected links to provide
					// at least a minimal contribution to the recommended value yet-to-be-fulfilled.
					//
					// This requirement is currently 5% of the remaining-to-be-collected value.
					// This means as we successfully advance in our collection,
					// the absolute liquidity contribution is lowered,
					// thus increasing the number of potential channels to be selected.

					// Derive the minimal liquidity contribution with a ratio of 20 (5%, rounded up)
					// or 100% if we're not allowed to do multipath payments.
					let minimal_value_contribution_msat: u64 = if allow_mpp {
						(recommended_value_msat - already_collected_value_msat + 19) / 20
					} else {
						final_value_msat
					};
					// Verify the liquidity offered by this channel complies to the minimal contribution.
					let contributes_sufficient_value = available_value_contribution_msat >= minimal_value_contribution_msat;

					let value_contribution_msat = cmp::min(available_value_contribution_msat, $next_hops_value_contribution);
					// Includes paying fees for the use of the following channels.
					let amount_to_transfer_over_msat: u64 = match value_contribution_msat.checked_add($next_hops_fee_msat) {
						Some(result) => result,
						// Can't overflow due to how the values were computed right above.
						None => unreachable!(),
					};

					// If HTLC minimum is larger than the amount we're going to transfer, we shouldn't
					// bother considering this channel.
					// Since we're choosing amount_to_transfer_over_msat as maximum possible, it can
					// be only reduced later (not increased), so this channel should just be skipped
					// as not sufficient.
					// TODO: Explore simply adding fee to hit htlc_minimum_msat
					if contributes_sufficient_value && amount_to_transfer_over_msat >= $directional_info.htlc_minimum_msat {
						// Note that low contribution here (limited by available_liquidity_msat)
						// might violate htlc_minimum_msat on the hops which are next along the
						// payment path (upstream to the payee). To avoid that, we recompute path
						// path fees knowing the final path contribution after constructing it.
						let hm_entry = dist.entry(&$src_node_id);
						let old_entry = hm_entry.or_insert_with(|| {
							// If there was previously no known way to access
							// the source node (recall it goes payee-to-payer) of $chan_id, first add
							// a semi-dummy record just to compute the fees to reach the source node.
							// This will affect our decision on selecting $chan_id
							// as a way to reach the $dest_node_id.
							let mut fee_base_msat = u32::max_value();
							let mut fee_proportional_millionths = u32::max_value();
							if let Some(Some(fees)) = network.get_nodes().get(&$src_node_id).map(|node| node.lowest_inbound_channel_fees) {
								fee_base_msat = fees.base_msat;
								fee_proportional_millionths = fees.proportional_millionths;
							}
							PathBuildingHop {
								route_hop: RouteHop {
									pubkey: $dest_node_id.clone(),
									node_features: NodeFeatures::empty(),
									short_channel_id: 0,
									channel_features: $chan_features.clone(),
									fee_msat: 0,
									cltv_expiry_delta: 0,
								},
								src_lowest_inbound_fees: RoutingFees {
									base_msat: fee_base_msat,
									proportional_millionths: fee_proportional_millionths,
								},
								channel_fees: $directional_info.fees,
								next_hops_fee_msat: u64::max_value(),
								hop_use_fee_msat: u64::max_value(),
								total_fee_msat: u64::max_value(),
								htlc_minimum_msat: $directional_info.htlc_minimum_msat,
							}
						});

						let mut hop_use_fee_msat = 0;
						let mut total_fee_msat = $next_hops_fee_msat;

						// Ignore hop_use_fee_msat for channel-from-us as we assume all channels-from-us
						// will have the same effective-fee
						if $src_node_id != *our_node_id {
							match compute_fees(amount_to_transfer_over_msat, $directional_info.fees) {
								// max_value means we'll always fail
								// the old_entry.total_fee_msat > total_fee_msat check
								None => total_fee_msat = u64::max_value(),
								Some(fee_msat) => {
									hop_use_fee_msat = fee_msat;
									total_fee_msat += hop_use_fee_msat;
									if let Some(prev_hop_fee_msat) = compute_fees(total_fee_msat + amount_to_transfer_over_msat,
																				old_entry.src_lowest_inbound_fees) {
										if let Some(incremented_total_fee_msat) = total_fee_msat.checked_add(prev_hop_fee_msat) {
											total_fee_msat = incremented_total_fee_msat;
										}
										else {
											// max_value means we'll always fail
											// the old_entry.total_fee_msat > total_fee_msat check
											total_fee_msat = u64::max_value();
										}
									} else {
										// max_value means we'll always fail
										// the old_entry.total_fee_msat > total_fee_msat check
										total_fee_msat = u64::max_value();
									}
								}
							}
						}

						let new_graph_node = RouteGraphNode {
							pubkey: $src_node_id,
							lowest_fee_to_peer_through_node: total_fee_msat,
							lowest_fee_to_node: $next_hops_fee_msat as u64 + hop_use_fee_msat,
							value_contribution_msat: value_contribution_msat,
						};

						// Update the way of reaching $src_node_id with the given $chan_id (from $dest_node_id),
						// if this way is cheaper than the already known
						// (considering the cost to "reach" this channel from the route destination,
						// the cost of using this channel,
						// and the cost of routing to the source node of this channel).
						// Also, consider that htlc_minimum_msat_difference, because we might end up
						// paying it. Consider the following exploit:
						// we use 2 paths to transfer 1.5 BTC. One of them is 0-fee normal 1 BTC path,
						// and for the other one we picked a 1sat-fee path with htlc_minimum_msat of
						// 1 BTC. Now, since the latter is more expensive, we gonna try to cut it
						// by 0.5 BTC, but then match htlc_minimum_msat by paying a fee of 0.5 BTC
						// to this channel.
						// TODO: this scoring could be smarter (e.g. 0.5*htlc_minimum_msat here).
						let mut old_cost = old_entry.total_fee_msat;
						if let Some(increased_old_cost) = old_cost.checked_add(old_entry.htlc_minimum_msat) {
							old_cost = increased_old_cost;
						} else {
							old_cost = u64::max_value();
						}

						let mut new_cost = total_fee_msat;
						if let Some(increased_new_cost) = new_cost.checked_add($directional_info.htlc_minimum_msat) {
							new_cost = increased_new_cost;
						} else {
							new_cost = u64::max_value();
						}

						if new_cost < old_cost {
							targets.push(new_graph_node);
							old_entry.next_hops_fee_msat = $next_hops_fee_msat;
							old_entry.hop_use_fee_msat = hop_use_fee_msat;
							old_entry.total_fee_msat = total_fee_msat;
							old_entry.route_hop = RouteHop {
								pubkey: $dest_node_id.clone(),
								node_features: NodeFeatures::empty(),
								short_channel_id: $chan_id.clone(),
								channel_features: $chan_features.clone(),
								fee_msat: 0, // This value will be later filled with hop_use_fee_msat of the following channel
								cltv_expiry_delta: $directional_info.cltv_expiry_delta as u32,
							};
							old_entry.channel_fees = $directional_info.fees;
							// It's probably fine to replace the old entry, because the new one
							// passed the htlc_minimum-related checks above.
							old_entry.htlc_minimum_msat = $directional_info.htlc_minimum_msat;
						}
					}
				}
			}
		};
	}

	// Find ways (channels with destination) to reach a given node and store them
	// in the corresponding data structures (routing graph etc).
	// $fee_to_target_msat represents how much it costs to reach to this node from the payee,
	// meaning how much will be paid in fees after this node (to the best of our knowledge).
	// This data can later be helpful to optimize routing (pay lower fees).
	macro_rules! add_entries_to_cheapest_to_target_node {
		( $node: expr, $node_id: expr, $fee_to_target_msat: expr, $next_hops_value_contribution: expr ) => {
			if first_hops.is_some() {
				if let Some(&(ref first_hop, ref features, ref outbound_capacity_msat)) = first_hop_targets.get(&$node_id) {
					add_entry!(first_hop, *our_node_id, $node_id, dummy_directional_info, Some(outbound_capacity_msat / 1000), features.to_context(), $fee_to_target_msat, $next_hops_value_contribution);
				}
			}

			let features;
			if let Some(node_info) = $node.announcement_info.as_ref() {
				features = node_info.features.clone();
			} else {
				features = NodeFeatures::empty();
			}

			if !features.requires_unknown_bits() {
				for chan_id in $node.channels.iter() {
					let chan = network.get_channels().get(chan_id).unwrap();
					if !chan.features.requires_unknown_bits() {
						if chan.node_one == *$node_id {
							// ie $node is one, ie next hop in A* is two, via the two_to_one channel
							if first_hops.is_none() || chan.node_two != *our_node_id {
								if let Some(two_to_one) = chan.two_to_one.as_ref() {
									if two_to_one.enabled {
										add_entry!(chan_id, chan.node_two, chan.node_one, two_to_one, chan.capacity_sats, chan.features, $fee_to_target_msat, $next_hops_value_contribution);
									}
								}
							}
						} else {
							if first_hops.is_none() || chan.node_one != *our_node_id {
								if let Some(one_to_two) = chan.one_to_two.as_ref() {
									if one_to_two.enabled {
										add_entry!(chan_id, chan.node_one, chan.node_two, one_to_two, chan.capacity_sats, chan.features, $fee_to_target_msat, $next_hops_value_contribution);
									}
								}

							}
						}
					}
				}
			}
		};
	}

	let mut payment_paths = Vec::<PaymentPath>::new();

	// TODO: diversify by nodes (so that all paths aren't doomed if one node is offline).
	'paths_collection: loop {
		// For every new path, start from scratch, except
		// bookkeeped_channels_liquidity_available_msat, which will improve
		// the further iterations of path finding. Also don't erase first_hop_targets.
		targets.clear();
		dist.clear();

		// If first hop is a private channel and the only way to reach the payee, this is the only
		// place where it could be added.
		if first_hops.is_some() {
			if let Some(&(ref first_hop, ref features, ref outbound_capacity_msat)) = first_hop_targets.get(&payee) {
				add_entry!(first_hop, *our_node_id, payee, dummy_directional_info, Some(outbound_capacity_msat / 1000), features.to_context(), 0, recommended_value_msat);
			}
		}

		// Add the payee as a target, so that the payee-to-payer
		// search algorithm knows what to start with.
		match network.get_nodes().get(payee) {
			// The payee is not in our network graph, so nothing to add here.
			// There is still a chance of reaching them via last_hops though,
			// so don't yet fail the payment here.
			// If not, targets.pop() will not even let us enter the loop in step 2.
			None => {},
			Some(node) => {
				add_entries_to_cheapest_to_target_node!(node, payee, 0, recommended_value_msat);
			},
		}

		// Step (1).
		// If a caller provided us with last hops, add them to routing targets. Since this happens
		// earlier than general path finding, they will be somewhat prioritized, although currently
		// it matters only if the fees are exactly the same.
		for hop in last_hops.iter() {
			let have_hop_src_in_graph =
				if let Some(&(ref first_hop, ref features, ref outbound_capacity_msat)) = first_hop_targets.get(&hop.src_node_id) {
					// If this hop connects to a node with which we have a direct channel, ignore
					// the network graph and add both the hop and our direct channel to
					// the candidate set.
					//
					// Currently there are no channel-context features defined, so we are a
					// bit lazy here. In the future, we should pull them out via our
					// ChannelManager, but there's no reason to waste the space until we
					// need them.
					add_entry!(first_hop, *our_node_id , hop.src_node_id, dummy_directional_info, Some(outbound_capacity_msat / 1000), features.to_context(), 0, recommended_value_msat);
					true
				} else {
					// In any other case, only add the hop if the source is in the regular network
					// graph:
					network.get_nodes().get(&hop.src_node_id).is_some()
				};
			if have_hop_src_in_graph {
				// BOLT 11 doesn't allow inclusion of features for the last hop hints, which
				// really sucks, cause we're gonna need that eventually.
				let last_hop_htlc_minimum_msat: u64 = match hop.htlc_minimum_msat {
					Some(htlc_minimum_msat) => htlc_minimum_msat,
					None => 0
				};
				let directional_info = DummyDirectionalChannelInfo {
					cltv_expiry_delta: hop.cltv_expiry_delta as u32,
					htlc_minimum_msat: last_hop_htlc_minimum_msat,
					htlc_maximum_msat: hop.htlc_maximum_msat,
					fees: hop.fees,
				};
				add_entry!(hop.short_channel_id, hop.src_node_id, payee, directional_info, None::<u64>, ChannelFeatures::empty(), 0, recommended_value_msat);
			}
		}

		// At this point, targets are filled with the data from first and
		// last hops communicated by the caller, and the payment receiver.
		let mut found_new_path = false;

		// Step (2).
		// If this loop terminates due the exhaustion of targets, two situations are possible:
		// - not enough outgoing liquidity:
		//   0 < already_collected_value_msat < final_value_msat
		// - enough outgoing liquidity:
		//   final_value_msat <= already_collected_value_msat < recommended_value_msat
		// Both these cases (and other cases except reaching recommended_value_msat) mean that
		// paths_collection will be stopped because found_new_path==false.
		// This is not necessarily a routing failure.
		'path_construction: while let Some(RouteGraphNode { pubkey, lowest_fee_to_node, value_contribution_msat, .. }) = targets.pop() {

			// Since we're going payee-to-payer, hitting our node as a target means we should stop
			// traversing the graph and arrange the path out of what we found.
			if pubkey == *our_node_id {
				let mut new_entry = dist.remove(&our_node_id).unwrap();
				let mut ordered_hops = vec!(new_entry.clone());

				'path_walk: loop {
					if let Some(&(_, ref features, _)) = first_hop_targets.get(&ordered_hops.last().unwrap().route_hop.pubkey) {
						ordered_hops.last_mut().unwrap().route_hop.node_features = features.to_context();
					} else if let Some(node) = network.get_nodes().get(&ordered_hops.last().unwrap().route_hop.pubkey) {
						if let Some(node_info) = node.announcement_info.as_ref() {
							ordered_hops.last_mut().unwrap().route_hop.node_features = node_info.features.clone();
						} else {
							ordered_hops.last_mut().unwrap().route_hop.node_features = NodeFeatures::empty();
						}
					} else {
						// We should be able to fill in features for everything except the last
						// hop, if the last hop was provided via a BOLT 11 invoice (though we
						// should be able to extend it further as BOLT 11 does have feature
						// flags for the last hop node itself).
						assert!(ordered_hops.last().unwrap().route_hop.pubkey == *payee);
					}

					// Means we succesfully traversed from the payer to the payee, now
					// save this path for the payment route. Also, update the liquidity
					// remaining on the used hops, so that we take them into account
					// while looking for more paths.
					if ordered_hops.last().unwrap().route_hop.pubkey == *payee {
						break 'path_walk;
					}

					new_entry = match dist.remove(&ordered_hops.last().unwrap().route_hop.pubkey) {
						Some(payment_hop) => payment_hop,
						// We can't arrive at None because, if we ever add an entry to targets,
						// we also fill in the entry in dist (see add_entry!).
						None => unreachable!(),
					};
					// We "propagate" the fees one hop backward (topologically) here,
					// so that fees paid for a HTLC forwarding on the current channel are
					// associated with the previous channel (where they will be subtracted).
					ordered_hops.last_mut().unwrap().route_hop.fee_msat = new_entry.hop_use_fee_msat;
					ordered_hops.last_mut().unwrap().route_hop.cltv_expiry_delta = new_entry.route_hop.cltv_expiry_delta;
					ordered_hops.push(new_entry.clone());
				}
				ordered_hops.last_mut().unwrap().route_hop.fee_msat = value_contribution_msat;
				ordered_hops.last_mut().unwrap().hop_use_fee_msat = 0;
				ordered_hops.last_mut().unwrap().route_hop.cltv_expiry_delta = final_cltv;

				let mut payment_path = PaymentPath {hops: ordered_hops};

				// We could have possibly constructed a slightly inconsistent path: since we reduce
				// value being transferred along the way, we could have violated htlc_minimum_msat
				// on some channels we already passed (assuming dest->source direction). Here, we
				// recompute the fees again, so that if that's the case, we match the currently
				// underpaid htlc_minimum_msat with fees.
				payment_path.update_value_and_recompute_fees(value_contribution_msat);

				// Since a path allows to transfer as much value as
				// the smallest channel it has ("bottleneck"), we should recompute
				// the fees so sender HTLC don't overpay fees when traversing
				// larger channels than the bottleneck. This may happen because
				// when we were selecting those channels we were not aware how much value
				// this path will transfer, and the relative fee for them
				// might have been computed considering a larger value.
				// Remember that we used these channels so that we don't rely
				// on the same liquidity in future paths.
				for payment_hop in payment_path.hops.iter() {
					let channel_liquidity_available_msat = bookkeeped_channels_liquidity_available_msat.get_mut(&payment_hop.route_hop.short_channel_id).unwrap();
					let mut spent_on_hop_msat = value_contribution_msat;
					let next_hops_fee_msat = payment_hop.next_hops_fee_msat;
					spent_on_hop_msat += next_hops_fee_msat;
					if *channel_liquidity_available_msat < spent_on_hop_msat {
						// This should not happen because we do recompute fees right before,
						// trying to avoid cases when a hop is not usable due to the fee situation.
						break 'path_construction;
					}
					*channel_liquidity_available_msat -= spent_on_hop_msat;
				}
				// Track the total amount all our collected paths allow to send so that we:
				// - know when to stop looking for more paths
				// - know which of the hops are useless considering how much more sats we need
				//   (contributes_sufficient_value)
				already_collected_value_msat += value_contribution_msat;

				payment_paths.push(payment_path);
				found_new_path = true;
				break 'path_construction;
			}

			// Otherwise, since the current target node is not us,
			// keep "unrolling" the payment graph from payee to payer by
			// finding a way to reach the current target from the payer side.
			match network.get_nodes().get(&pubkey) {
				None => {},
				Some(node) => {
					add_entries_to_cheapest_to_target_node!(node, &pubkey, lowest_fee_to_node, value_contribution_msat);
				},
			}
		}

		if !allow_mpp {
			// If we don't support MPP, no use trying to gather more value ever.
			break 'paths_collection;
		}

		// Step (3).
		// Stop either when recommended value is reached,
		// or if during last iteration no new path was found.
		// In the latter case, making another path finding attempt could not help,
		// because we deterministically terminate the search due to low liquidity.
		if already_collected_value_msat >= recommended_value_msat || !found_new_path {
			break 'paths_collection;
		}
	}

	// Step (4).
	if payment_paths.len() == 0 {
		return Err(LightningError{err: "Failed to find a path to the given destination".to_owned(), action: ErrorAction::IgnoreError});
	}

	if already_collected_value_msat < final_value_msat {
		return Err(LightningError{err: "Failed to find a sufficient route to the given destination".to_owned(), action: ErrorAction::IgnoreError});
	}

	// Sort by total fees and take the best paths.
	payment_paths.sort_by_key(|path| path.get_total_fee_paid_msat());
	if payment_paths.len() > 50 {
		payment_paths.truncate(50);
	}

	// Draw multiple sufficient routes by randomly combining the selected paths.
	let mut drawn_routes = Vec::new();
	for i in 0..payment_paths.len() {
		let mut cur_route = Vec::<PaymentPath>::new();
		let mut aggregate_route_value_msat = 0;

		// Step (5).
		// TODO: real random shuffle
		// Currently just starts with i_th and goes up to i-1_th in a looped way.
		let cur_payment_paths = [&payment_paths[i..], &payment_paths[..i]].concat();

		// Step (6).
		for payment_path in cur_payment_paths {
			cur_route.push(payment_path.clone());
			aggregate_route_value_msat += payment_path.get_value_msat();
			if aggregate_route_value_msat > final_value_msat {
				// Last path likely overpaid. Substract it from the most expensive
				// (in terms of proportional fee) path in this route and recompute fees.
				// This might be not the most economically efficient way, but fewer paths
				// also makes routing more reliable.
				let mut overpaid_value_msat = aggregate_route_value_msat - final_value_msat;

				// First, drop some expensive low-value paths entirely if possible.
				// Sort by value so that we drop many really-low values first, since
				// fewer paths is better: the payment is less likely to fail.
				// TODO: this could also be optimized by also sorting by feerate_per_sat_routed,
				// so that the sender pays less fees overall. And also htlc_minimum_msat.
				cur_route.sort_by_key(|path| path.get_value_msat());
				// We should make sure that at least 1 path left.
				let mut paths_left = cur_route.len();
				cur_route.retain(|path| {
					if paths_left == 1 {
						return true
					}
					let mut keep = true;
					let path_value_msat = path.get_value_msat();
					if path_value_msat <= overpaid_value_msat {
						keep = false;
						overpaid_value_msat -= path_value_msat;
						paths_left -= 1;
					}
					keep
				});

				if overpaid_value_msat == 0 {
					break;
				}

				assert!(cur_route.len() > 0);

				// Step (7).
				// Now, substract the overpaid value from the most-expensive path.
				// TODO: this could also be optimized by also sorting by feerate_per_sat_routed,
				// so that the sender pays less fees overall. And also htlc_minimum_msat.
				cur_route.sort_by_key(|path| { path.hops.iter().map(|hop| hop.channel_fees.proportional_millionths as u64).sum::<u64>() });
				let expensive_payment_path = cur_route.first_mut().unwrap();
				// We already dropped all the small channels above, meaning all the
				// remaining channels are larger than remaining overpaid_value_msat.
				// Thus, this can't be negative.
				let expensive_path_new_value_msat = expensive_payment_path.get_value_msat() - overpaid_value_msat;
				expensive_payment_path.update_value_and_recompute_fees(expensive_path_new_value_msat);
				break;
			}
		}
		drawn_routes.push(cur_route);
	}

	// Step (8).
	// Select the best route by lowest total fee.
	drawn_routes.sort_by_key(|paths| paths.iter().map(|path| path.get_total_fee_paid_msat()).sum::<u64>());
	let mut selected_paths = Vec::<Vec<RouteHop>>::new();
	for payment_path in drawn_routes.first().unwrap() {
		selected_paths.push(payment_path.hops.iter().map(|payment_hop| payment_hop.route_hop.clone()).collect());
	}

	if let Some(features) = &payee_features {
		for path in selected_paths.iter_mut() {
			path.last_mut().unwrap().node_features = features.to_context();
		}
	}

	let route = Route { paths: selected_paths };
	log_trace!(logger, "Got route: {}", log_route!(route));
	Ok(route)
}

#[cfg(test)]
mod tests {
	use routing::router::{get_route, RouteHint, RoutingFees};
	use routing::network_graph::{NetworkGraph, NetGraphMsgHandler};
	use ln::features::{ChannelFeatures, InitFeatures, InvoiceFeatures, NodeFeatures};
	use ln::msgs::{ErrorAction, LightningError, OptionalField, UnsignedChannelAnnouncement, ChannelAnnouncement, RoutingMessageHandler,
	   NodeAnnouncement, UnsignedNodeAnnouncement, ChannelUpdate, UnsignedChannelUpdate};
	use ln::channelmanager;
	use util::test_utils;
	use util::ser::Writeable;

	use bitcoin::hashes::sha256d::Hash as Sha256dHash;
	use bitcoin::hashes::Hash;
	use bitcoin::network::constants::Network;
	use bitcoin::blockdata::constants::genesis_block;
	use bitcoin::blockdata::script::Builder;
	use bitcoin::blockdata::opcodes;
	use bitcoin::blockdata::transaction::TxOut;

	use hex;

	use bitcoin::secp256k1::key::{PublicKey,SecretKey};
	use bitcoin::secp256k1::{Secp256k1, All};

	use std::sync::Arc;

	// Using the same keys for LN and BTC ids
	fn add_channel(net_graph_msg_handler: &NetGraphMsgHandler<Arc<test_utils::TestChainSource>, Arc<test_utils::TestLogger>>, secp_ctx: &Secp256k1<All>, node_1_privkey: &SecretKey,
	   node_2_privkey: &SecretKey, features: ChannelFeatures, short_channel_id: u64) {
		let node_id_1 = PublicKey::from_secret_key(&secp_ctx, node_1_privkey);
		let node_id_2 = PublicKey::from_secret_key(&secp_ctx, node_2_privkey);

		let unsigned_announcement = UnsignedChannelAnnouncement {
			features,
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id,
			node_id_1,
			node_id_2,
			bitcoin_key_1: node_id_1,
			bitcoin_key_2: node_id_2,
			excess_data: Vec::new(),
		};

		let msghash = hash_to_message!(&Sha256dHash::hash(&unsigned_announcement.encode()[..])[..]);
		let valid_announcement = ChannelAnnouncement {
			node_signature_1: secp_ctx.sign(&msghash, node_1_privkey),
			node_signature_2: secp_ctx.sign(&msghash, node_2_privkey),
			bitcoin_signature_1: secp_ctx.sign(&msghash, node_1_privkey),
			bitcoin_signature_2: secp_ctx.sign(&msghash, node_2_privkey),
			contents: unsigned_announcement.clone(),
		};
		match net_graph_msg_handler.handle_channel_announcement(&valid_announcement) {
			Ok(res) => assert!(res),
			_ => panic!()
		};
	}

	fn update_channel(net_graph_msg_handler: &NetGraphMsgHandler<Arc<test_utils::TestChainSource>, Arc<test_utils::TestLogger>>, secp_ctx: &Secp256k1<All>, node_privkey: &SecretKey, update: UnsignedChannelUpdate) {
		let msghash = hash_to_message!(&Sha256dHash::hash(&update.encode()[..])[..]);
		let valid_channel_update = ChannelUpdate {
			signature: secp_ctx.sign(&msghash, node_privkey),
			contents: update.clone()
		};

		match net_graph_msg_handler.handle_channel_update(&valid_channel_update) {
			Ok(res) => assert!(res),
			Err(_) => panic!()
		};
	}

	fn add_or_update_node(net_graph_msg_handler: &NetGraphMsgHandler<Arc<test_utils::TestChainSource>, Arc<test_utils::TestLogger>>, secp_ctx: &Secp256k1<All>, node_privkey: &SecretKey,
	   features: NodeFeatures, timestamp: u32) {
		let node_id = PublicKey::from_secret_key(&secp_ctx, node_privkey);
		let unsigned_announcement = UnsignedNodeAnnouncement {
			features,
			timestamp,
			node_id,
			rgb: [0; 3],
			alias: [0; 32],
			addresses: Vec::new(),
			excess_address_data: Vec::new(),
			excess_data: Vec::new(),
		};
		let msghash = hash_to_message!(&Sha256dHash::hash(&unsigned_announcement.encode()[..])[..]);
		let valid_announcement = NodeAnnouncement {
			signature: secp_ctx.sign(&msghash, node_privkey),
			contents: unsigned_announcement.clone()
		};

		match net_graph_msg_handler.handle_node_announcement(&valid_announcement) {
			Ok(_) => (),
			Err(_) => panic!()
		};
	}

	fn get_nodes(secp_ctx: &Secp256k1<All>) -> (SecretKey, PublicKey, Vec<SecretKey>, Vec<PublicKey>) {
		let privkeys: Vec<SecretKey> = (2..10).map(|i| {
			SecretKey::from_slice(&hex::decode(format!("{:02}", i).repeat(32)).unwrap()[..]).unwrap()
		}).collect();

		let pubkeys = privkeys.iter().map(|secret| PublicKey::from_secret_key(&secp_ctx, secret)).collect();

		let our_privkey = SecretKey::from_slice(&hex::decode("01".repeat(32)).unwrap()[..]).unwrap();
		let our_id = PublicKey::from_secret_key(&secp_ctx, &our_privkey);

		(our_privkey, our_id, privkeys, pubkeys)
	}

	fn id_to_feature_flags(id: u8) -> Vec<u8> {
		// Set the feature flags to the id'th odd (ie non-required) feature bit so that we can
		// test for it later.
		let idx = (id - 1) * 2 + 1;
		if idx > 8*3 {
			vec![1 << (idx - 8*3), 0, 0, 0]
		} else if idx > 8*2 {
			vec![1 << (idx - 8*2), 0, 0]
		} else if idx > 8*1 {
			vec![1 << (idx - 8*1), 0]
		} else {
			vec![1 << idx]
		}
	}

	fn build_graph() -> (Secp256k1<All>, NetGraphMsgHandler<std::sync::Arc<test_utils::TestChainSource>, std::sync::Arc<crate::util::test_utils::TestLogger>>, std::sync::Arc<test_utils::TestChainSource>, std::sync::Arc<test_utils::TestLogger>) {
		let secp_ctx = Secp256k1::new();
		let logger = Arc::new(test_utils::TestLogger::new());
		let chain_monitor = Arc::new(test_utils::TestChainSource::new(Network::Testnet));
		let net_graph_msg_handler = NetGraphMsgHandler::new(genesis_block(Network::Testnet).header.block_hash(), None, Arc::clone(&logger));
		// Build network from our_id to node7:
		//
		//        -1(1)2-  node0  -1(3)2-
		//       /                       \
		// our_id -1(12)2- node7 -1(13)2--- node2
		//       \                       /
		//        -1(2)2-  node1  -1(4)2-
		//
		//
		// chan1  1-to-2: disabled
		// chan1  2-to-1: enabled, 0 fee
		//
		// chan2  1-to-2: enabled, ignored fee
		// chan2  2-to-1: enabled, 0 fee
		//
		// chan3  1-to-2: enabled, 0 fee
		// chan3  2-to-1: enabled, 100 msat fee
		//
		// chan4  1-to-2: enabled, 100% fee
		// chan4  2-to-1: enabled, 0 fee
		//
		// chan12 1-to-2: enabled, ignored fee
		// chan12 2-to-1: enabled, 0 fee
		//
		// chan13 1-to-2: enabled, 200% fee
		// chan13 2-to-1: enabled, 0 fee
		//
		//
		//       -1(5)2- node3 -1(8)2--
		//       |         2          |
		//       |       (11)         |
		//      /          1           \
		// node2--1(6)2- node4 -1(9)2--- node6 (not in global route map)
		//      \                      /
		//       -1(7)2- node5 -1(10)2-
		//
		// chan5  1-to-2: enabled, 100 msat fee
		// chan5  2-to-1: enabled, 0 fee
		//
		// chan6  1-to-2: enabled, 0 fee
		// chan6  2-to-1: enabled, 0 fee
		//
		// chan7  1-to-2: enabled, 100% fee
		// chan7  2-to-1: enabled, 0 fee
		//
		// chan8  1-to-2: enabled, variable fee (0 then 1000 msat)
		// chan8  2-to-1: enabled, 0 fee
		//
		// chan9  1-to-2: enabled, 1001 msat fee
		// chan9  2-to-1: enabled, 0 fee
		//
		// chan10 1-to-2: enabled, 0 fee
		// chan10 2-to-1: enabled, 0 fee
		//
		// chan11 1-to-2: enabled, 0 fee
		// chan11 2-to-1: enabled, 0 fee

		let (our_privkey, _, privkeys, _) = get_nodes(&secp_ctx);

		add_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, &privkeys[0], ChannelFeatures::from_le_bytes(id_to_feature_flags(1)), 1);
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[0], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 1,
			timestamp: 1,
			flags: 1,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		add_or_update_node(&net_graph_msg_handler, &secp_ctx, &privkeys[0], NodeFeatures::from_le_bytes(id_to_feature_flags(1)), 0);

		add_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, &privkeys[1], ChannelFeatures::from_le_bytes(id_to_feature_flags(2)), 2);
		update_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 2,
			timestamp: 1,
			flags: 0,
			cltv_expiry_delta: u16::max_value(),
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: u32::max_value(),
			fee_proportional_millionths: u32::max_value(),
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[1], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 2,
			timestamp: 1,
			flags: 1,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		add_or_update_node(&net_graph_msg_handler, &secp_ctx, &privkeys[1], NodeFeatures::from_le_bytes(id_to_feature_flags(2)), 0);

		add_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, &privkeys[7], ChannelFeatures::from_le_bytes(id_to_feature_flags(12)), 12);
		update_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 12,
			timestamp: 1,
			flags: 0,
			cltv_expiry_delta: u16::max_value(),
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: u32::max_value(),
			fee_proportional_millionths: u32::max_value(),
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[7], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 12,
			timestamp: 1,
			flags: 1,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		add_or_update_node(&net_graph_msg_handler, &secp_ctx, &privkeys[7], NodeFeatures::from_le_bytes(id_to_feature_flags(8)), 0);

		add_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[0], &privkeys[2], ChannelFeatures::from_le_bytes(id_to_feature_flags(3)), 3);
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[0], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 3,
			timestamp: 1,
			flags: 0,
			cltv_expiry_delta: (3 << 8) | 1,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[2], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 3,
			timestamp: 1,
			flags: 1,
			cltv_expiry_delta: (3 << 8) | 2,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 100,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		add_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[1], &privkeys[2], ChannelFeatures::from_le_bytes(id_to_feature_flags(4)), 4);
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[1], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 4,
			timestamp: 1,
			flags: 0,
			cltv_expiry_delta: (4 << 8) | 1,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 1000000,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[2], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 4,
			timestamp: 1,
			flags: 1,
			cltv_expiry_delta: (4 << 8) | 2,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		add_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[7], &privkeys[2], ChannelFeatures::from_le_bytes(id_to_feature_flags(13)), 13);
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[7], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 13,
			timestamp: 1,
			flags: 0,
			cltv_expiry_delta: (13 << 8) | 1,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 2000000,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[2], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 13,
			timestamp: 1,
			flags: 1,
			cltv_expiry_delta: (13 << 8) | 2,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		add_or_update_node(&net_graph_msg_handler, &secp_ctx, &privkeys[2], NodeFeatures::from_le_bytes(id_to_feature_flags(3)), 0);

		add_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[2], &privkeys[4], ChannelFeatures::from_le_bytes(id_to_feature_flags(6)), 6);
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[2], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 6,
			timestamp: 1,
			flags: 0,
			cltv_expiry_delta: (6 << 8) | 1,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[4], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 6,
			timestamp: 1,
			flags: 1,
			cltv_expiry_delta: (6 << 8) | 2,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new(),
		});

		add_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[4], &privkeys[3], ChannelFeatures::from_le_bytes(id_to_feature_flags(11)), 11);
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[4], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 11,
			timestamp: 1,
			flags: 0,
			cltv_expiry_delta: (11 << 8) | 1,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[3], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 11,
			timestamp: 1,
			flags: 1,
			cltv_expiry_delta: (11 << 8) | 2,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		add_or_update_node(&net_graph_msg_handler, &secp_ctx, &privkeys[4], NodeFeatures::from_le_bytes(id_to_feature_flags(5)), 0);

		add_or_update_node(&net_graph_msg_handler, &secp_ctx, &privkeys[3], NodeFeatures::from_le_bytes(id_to_feature_flags(4)), 0);

		add_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[2], &privkeys[5], ChannelFeatures::from_le_bytes(id_to_feature_flags(7)), 7);
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[2], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 7,
			timestamp: 1,
			flags: 0,
			cltv_expiry_delta: (7 << 8) | 1,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 1000000,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[5], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 7,
			timestamp: 1,
			flags: 1,
			cltv_expiry_delta: (7 << 8) | 2,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		add_or_update_node(&net_graph_msg_handler, &secp_ctx, &privkeys[5], NodeFeatures::from_le_bytes(id_to_feature_flags(6)), 0);

		(secp_ctx, net_graph_msg_handler, chain_monitor, logger)
	}

	#[test]
	fn simple_route_test() {
		let (secp_ctx, net_graph_msg_handler, _, logger) = build_graph();
		let (_, our_id, _, nodes) = get_nodes(&secp_ctx);

		// Simple route to 2 via 1

		if let Err(LightningError{err, action: ErrorAction::IgnoreError}) = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[2], None, None, &Vec::new(), 0, 42, Arc::clone(&logger)) {
			assert_eq!(err, "Cannot send a payment of 0 msat");
		} else { panic!(); }

		let route = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[2], None, None, &Vec::new(), 100, 42, Arc::clone(&logger)).unwrap();
		assert_eq!(route.paths[0].len(), 2);

		assert_eq!(route.paths[0][0].pubkey, nodes[1]);
		assert_eq!(route.paths[0][0].short_channel_id, 2);
		assert_eq!(route.paths[0][0].fee_msat, 100);
		assert_eq!(route.paths[0][0].cltv_expiry_delta, (4 << 8) | 1);
		assert_eq!(route.paths[0][0].node_features.le_flags(), &id_to_feature_flags(2));
		assert_eq!(route.paths[0][0].channel_features.le_flags(), &id_to_feature_flags(2));

		assert_eq!(route.paths[0][1].pubkey, nodes[2]);
		assert_eq!(route.paths[0][1].short_channel_id, 4);
		assert_eq!(route.paths[0][1].fee_msat, 100);
		assert_eq!(route.paths[0][1].cltv_expiry_delta, 42);
		assert_eq!(route.paths[0][1].node_features.le_flags(), &id_to_feature_flags(3));
		assert_eq!(route.paths[0][1].channel_features.le_flags(), &id_to_feature_flags(4));
	}

	#[test]
	fn invalid_first_hop_test() {
		let (secp_ctx, net_graph_msg_handler, _, logger) = build_graph();
		let (_, our_id, _, nodes) = get_nodes(&secp_ctx);

		// Simple route to 2 via 1

		let our_chans = vec![channelmanager::ChannelDetails {
			channel_id: [0; 32],
			short_channel_id: Some(2),
			remote_network_id: our_id,
			counterparty_features: InitFeatures::from_le_bytes(vec![0b11]),
			channel_value_satoshis: 100000,
			user_id: 0,
			outbound_capacity_msat: 100000,
			inbound_capacity_msat: 100000,
			is_live: true,
			counterparty_forwarding_info: None,
		}];

		if let Err(LightningError{err, action: ErrorAction::IgnoreError}) = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[2], None, Some(&our_chans.iter().collect::<Vec<_>>()), &Vec::new(), 100, 42, Arc::clone(&logger)) {
			assert_eq!(err, "First hop cannot have our_node_id as a destination.");
		} else { panic!(); }

		let route = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[2], None, None, &Vec::new(), 100, 42, Arc::clone(&logger)).unwrap();
		assert_eq!(route.paths[0].len(), 2);
	}

	#[test]
	fn htlc_minimum_test() {
		let (secp_ctx, net_graph_msg_handler, _, logger) = build_graph();
		let (our_privkey, our_id, privkeys, nodes) = get_nodes(&secp_ctx);

		// Simple route to 2 via 1

		// Disable other paths
		update_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 12,
			timestamp: 2,
			flags: 2, // to disable
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[0], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 3,
			timestamp: 2,
			flags: 2, // to disable
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[7], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 13,
			timestamp: 2,
			flags: 2, // to disable
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[2], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 6,
			timestamp: 2,
			flags: 2, // to disable
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[2], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 7,
			timestamp: 2,
			flags: 2, // to disable
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		// Check against amount_to_transfer_over_msat.
		// Set minimal HTLC of 200_000_000 msat.
		update_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 2,
			timestamp: 3,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 200_000_000,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		// Second hop only allows to forward 199_999_999 at most, thus not allowing the first hop to
		// be used.
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[1], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 4,
			timestamp: 3,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(199_999_999),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		// Not possible to send 199_999_999, because the minimum on channel=2 is 200_000_000.
		if let Err(LightningError{err, action: ErrorAction::IgnoreError}) = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[2], None, None, &Vec::new(), 199_999_999, 42, Arc::clone(&logger)) {
			assert_eq!(err, "Failed to find a path to the given destination");
		} else { panic!(); }

		// Lift the restriction on the first hop.
		update_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 2,
			timestamp: 4,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		// A payment above the minimum should pass
		let route = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[2], None, None, &Vec::new(), 199_999_999, 42, Arc::clone(&logger)).unwrap();
		assert_eq!(route.paths[0].len(), 2);
	}

	#[test]
	fn htlc_minimum_overpay_test() {
		let (secp_ctx, net_graph_msg_handler, _, logger) = build_graph();
		let (our_privkey, our_id, privkeys, nodes) = get_nodes(&secp_ctx);

		// A route to node#2 via two paths.
		// One path allows transferring 35-40 sats, another one also allows 35-40 sats.
		// Thus, they can't send 60 without overpaying.
		update_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 2,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 35_000,
			htlc_maximum_msat: OptionalField::Present(40_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 12,
			timestamp: 3,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 35_000,
			htlc_maximum_msat: OptionalField::Present(40_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		// Make 0 fee.
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[7], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 13,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[1], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 4,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		// Disable other paths
		update_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 1,
			timestamp: 3,
			flags: 2, // to disable
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		let route = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[2],
			Some(InvoiceFeatures::known()), None, &Vec::new(), 60_000, 42, Arc::clone(&logger)).unwrap();
		// Overpay fees to hit htlc_minimum_msat.
		let overpaid_fees = route.paths[0][0].fee_msat + route.paths[1][0].fee_msat;
		// TODO: this could be better balanced to overpay 10k and not 15k.
		assert_eq!(overpaid_fees, 15_000);

		// Now, test that if there are 2 paths, a "cheaper" by fee path wouldn't be prioritized
		// while taking even more fee to match htlc_minimum_msat.
		update_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 12,
			timestamp: 4,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 65_000,
			htlc_maximum_msat: OptionalField::Present(80_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 2,
			timestamp: 3,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[1], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 4,
			timestamp: 4,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 100_000,
			excess_data: Vec::new()
		});

		let route = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[2],
			Some(InvoiceFeatures::known()), None, &Vec::new(), 60_000, 42, Arc::clone(&logger)).unwrap();
		// Fine to overpay for htlc_minimum_msat if it allows us to save fee.
		assert_eq!(route.paths.len(), 1);
		assert_eq!(route.paths[0][0].short_channel_id, 12);
		let fees = route.paths[0][0].fee_msat;
		assert_eq!(fees, 5_000);

		let route = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[2],
			Some(InvoiceFeatures::known()), None, &Vec::new(), 50_000, 42, Arc::clone(&logger)).unwrap();
		// Not fine to overpay for htlc_minimum_msat if it requires paying more than fee on
		// the other channel.
		assert_eq!(route.paths.len(), 1);
		assert_eq!(route.paths[0][0].short_channel_id, 2);
		let fees = route.paths[0][0].fee_msat;
		assert_eq!(fees, 5_000);
	}

	#[test]
	fn disable_channels_test() {
		let (secp_ctx, net_graph_msg_handler, _, logger) = build_graph();
		let (our_privkey, our_id, privkeys, nodes) = get_nodes(&secp_ctx);

		// // Disable channels 4 and 12 by flags=2
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[1], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 4,
			timestamp: 2,
			flags: 2, // to disable
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 12,
			timestamp: 2,
			flags: 2, // to disable
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		// If all the channels require some features we don't understand, route should fail
		if let Err(LightningError{err, action: ErrorAction::IgnoreError}) = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[2], None, None, &Vec::new(), 100, 42, Arc::clone(&logger)) {
			assert_eq!(err, "Failed to find a path to the given destination");
		} else { panic!(); }

		// If we specify a channel to node7, that overrides our local channel view and that gets used
		let our_chans = vec![channelmanager::ChannelDetails {
			channel_id: [0; 32],
			short_channel_id: Some(42),
			remote_network_id: nodes[7].clone(),
			counterparty_features: InitFeatures::from_le_bytes(vec![0b11]),
			channel_value_satoshis: 0,
			user_id: 0,
			outbound_capacity_msat: 250_000_000,
			inbound_capacity_msat: 0,
			is_live: true,
			counterparty_forwarding_info: None,
		}];
		let route = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[2], None, Some(&our_chans.iter().collect::<Vec<_>>()),  &Vec::new(), 100, 42, Arc::clone(&logger)).unwrap();
		assert_eq!(route.paths[0].len(), 2);

		assert_eq!(route.paths[0][0].pubkey, nodes[7]);
		assert_eq!(route.paths[0][0].short_channel_id, 42);
		assert_eq!(route.paths[0][0].fee_msat, 200);
		assert_eq!(route.paths[0][0].cltv_expiry_delta, (13 << 8) | 1);
		assert_eq!(route.paths[0][0].node_features.le_flags(), &vec![0b11]); // it should also override our view of their features
		assert_eq!(route.paths[0][0].channel_features.le_flags(), &Vec::<u8>::new()); // No feature flags will meet the relevant-to-channel conversion

		assert_eq!(route.paths[0][1].pubkey, nodes[2]);
		assert_eq!(route.paths[0][1].short_channel_id, 13);
		assert_eq!(route.paths[0][1].fee_msat, 100);
		assert_eq!(route.paths[0][1].cltv_expiry_delta, 42);
		assert_eq!(route.paths[0][1].node_features.le_flags(), &id_to_feature_flags(3));
		assert_eq!(route.paths[0][1].channel_features.le_flags(), &id_to_feature_flags(13));
	}

	#[test]
	fn disable_node_test() {
		let (secp_ctx, net_graph_msg_handler, _, logger) = build_graph();
		let (_, our_id, privkeys, nodes) = get_nodes(&secp_ctx);

		// Disable nodes 1, 2, and 8 by requiring unknown feature bits
		let mut unknown_features = NodeFeatures::known();
		unknown_features.set_required_unknown_bits();
		add_or_update_node(&net_graph_msg_handler, &secp_ctx, &privkeys[0], unknown_features.clone(), 1);
		add_or_update_node(&net_graph_msg_handler, &secp_ctx, &privkeys[1], unknown_features.clone(), 1);
		add_or_update_node(&net_graph_msg_handler, &secp_ctx, &privkeys[7], unknown_features.clone(), 1);

		// If all nodes require some features we don't understand, route should fail
		if let Err(LightningError{err, action: ErrorAction::IgnoreError}) = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[2], None, None, &Vec::new(), 100, 42, Arc::clone(&logger)) {
			assert_eq!(err, "Failed to find a path to the given destination");
		} else { panic!(); }

		// If we specify a channel to node7, that overrides our local channel view and that gets used
		let our_chans = vec![channelmanager::ChannelDetails {
			channel_id: [0; 32],
			short_channel_id: Some(42),
			remote_network_id: nodes[7].clone(),
			counterparty_features: InitFeatures::from_le_bytes(vec![0b11]),
			channel_value_satoshis: 0,
			user_id: 0,
			outbound_capacity_msat: 250_000_000,
			inbound_capacity_msat: 0,
			is_live: true,
			counterparty_forwarding_info: None,
		}];
		let route = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[2], None, Some(&our_chans.iter().collect::<Vec<_>>()), &Vec::new(), 100, 42, Arc::clone(&logger)).unwrap();
		assert_eq!(route.paths[0].len(), 2);

		assert_eq!(route.paths[0][0].pubkey, nodes[7]);
		assert_eq!(route.paths[0][0].short_channel_id, 42);
		assert_eq!(route.paths[0][0].fee_msat, 200);
		assert_eq!(route.paths[0][0].cltv_expiry_delta, (13 << 8) | 1);
		assert_eq!(route.paths[0][0].node_features.le_flags(), &vec![0b11]); // it should also override our view of their features
		assert_eq!(route.paths[0][0].channel_features.le_flags(), &Vec::<u8>::new()); // No feature flags will meet the relevant-to-channel conversion

		assert_eq!(route.paths[0][1].pubkey, nodes[2]);
		assert_eq!(route.paths[0][1].short_channel_id, 13);
		assert_eq!(route.paths[0][1].fee_msat, 100);
		assert_eq!(route.paths[0][1].cltv_expiry_delta, 42);
		assert_eq!(route.paths[0][1].node_features.le_flags(), &id_to_feature_flags(3));
		assert_eq!(route.paths[0][1].channel_features.le_flags(), &id_to_feature_flags(13));

		// Note that we don't test disabling node 3 and failing to route to it, as we (somewhat
		// naively) assume that the user checked the feature bits on the invoice, which override
		// the node_announcement.
	}

	#[test]
	fn our_chans_test() {
		let (secp_ctx, net_graph_msg_handler, _, logger) = build_graph();
		let (_, our_id, _, nodes) = get_nodes(&secp_ctx);

		// Route to 1 via 2 and 3 because our channel to 1 is disabled
		let route = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[0], None, None, &Vec::new(), 100, 42, Arc::clone(&logger)).unwrap();
		assert_eq!(route.paths[0].len(), 3);

		assert_eq!(route.paths[0][0].pubkey, nodes[1]);
		assert_eq!(route.paths[0][0].short_channel_id, 2);
		assert_eq!(route.paths[0][0].fee_msat, 200);
		assert_eq!(route.paths[0][0].cltv_expiry_delta, (4 << 8) | 1);
		assert_eq!(route.paths[0][0].node_features.le_flags(), &id_to_feature_flags(2));
		assert_eq!(route.paths[0][0].channel_features.le_flags(), &id_to_feature_flags(2));

		assert_eq!(route.paths[0][1].pubkey, nodes[2]);
		assert_eq!(route.paths[0][1].short_channel_id, 4);
		assert_eq!(route.paths[0][1].fee_msat, 100);
		assert_eq!(route.paths[0][1].cltv_expiry_delta, (3 << 8) | 2);
		assert_eq!(route.paths[0][1].node_features.le_flags(), &id_to_feature_flags(3));
		assert_eq!(route.paths[0][1].channel_features.le_flags(), &id_to_feature_flags(4));

		assert_eq!(route.paths[0][2].pubkey, nodes[0]);
		assert_eq!(route.paths[0][2].short_channel_id, 3);
		assert_eq!(route.paths[0][2].fee_msat, 100);
		assert_eq!(route.paths[0][2].cltv_expiry_delta, 42);
		assert_eq!(route.paths[0][2].node_features.le_flags(), &id_to_feature_flags(1));
		assert_eq!(route.paths[0][2].channel_features.le_flags(), &id_to_feature_flags(3));

		// If we specify a channel to node7, that overrides our local channel view and that gets used
		let our_chans = vec![channelmanager::ChannelDetails {
			channel_id: [0; 32],
			short_channel_id: Some(42),
			remote_network_id: nodes[7].clone(),
			counterparty_features: InitFeatures::from_le_bytes(vec![0b11]),
			channel_value_satoshis: 0,
			user_id: 0,
			outbound_capacity_msat: 250_000_000,
			inbound_capacity_msat: 0,
			is_live: true,
			counterparty_forwarding_info: None,
		}];
		let route = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[2], None, Some(&our_chans.iter().collect::<Vec<_>>()), &Vec::new(), 100, 42, Arc::clone(&logger)).unwrap();
		assert_eq!(route.paths[0].len(), 2);

		assert_eq!(route.paths[0][0].pubkey, nodes[7]);
		assert_eq!(route.paths[0][0].short_channel_id, 42);
		assert_eq!(route.paths[0][0].fee_msat, 200);
		assert_eq!(route.paths[0][0].cltv_expiry_delta, (13 << 8) | 1);
		assert_eq!(route.paths[0][0].node_features.le_flags(), &vec![0b11]);
		assert_eq!(route.paths[0][0].channel_features.le_flags(), &Vec::<u8>::new()); // No feature flags will meet the relevant-to-channel conversion

		assert_eq!(route.paths[0][1].pubkey, nodes[2]);
		assert_eq!(route.paths[0][1].short_channel_id, 13);
		assert_eq!(route.paths[0][1].fee_msat, 100);
		assert_eq!(route.paths[0][1].cltv_expiry_delta, 42);
		assert_eq!(route.paths[0][1].node_features.le_flags(), &id_to_feature_flags(3));
		assert_eq!(route.paths[0][1].channel_features.le_flags(), &id_to_feature_flags(13));
	}

	fn last_hops(nodes: &Vec<PublicKey>) -> Vec<RouteHint> {
		let zero_fees = RoutingFees {
			base_msat: 0,
			proportional_millionths: 0,
		};
		vec!(RouteHint {
			src_node_id: nodes[3].clone(),
			short_channel_id: 8,
			fees: zero_fees,
			cltv_expiry_delta: (8 << 8) | 1,
			htlc_minimum_msat: None,
			htlc_maximum_msat: None,
		}, RouteHint {
			src_node_id: nodes[4].clone(),
			short_channel_id: 9,
			fees: RoutingFees {
				base_msat: 1001,
				proportional_millionths: 0,
			},
			cltv_expiry_delta: (9 << 8) | 1,
			htlc_minimum_msat: None,
			htlc_maximum_msat: None,
		}, RouteHint {
			src_node_id: nodes[5].clone(),
			short_channel_id: 10,
			fees: zero_fees,
			cltv_expiry_delta: (10 << 8) | 1,
			htlc_minimum_msat: None,
			htlc_maximum_msat: None,
		})
	}

	#[test]
	fn last_hops_test() {
		let (secp_ctx, net_graph_msg_handler, _, logger) = build_graph();
		let (_, our_id, _, nodes) = get_nodes(&secp_ctx);

		// Simple test across 2, 3, 5, and 4 via a last_hop channel

		// First check that lst hop can't have its source as the payee.
		let invalid_last_hop = RouteHint {
			src_node_id: nodes[6],
			short_channel_id: 8,
			fees: RoutingFees {
				base_msat: 1000,
				proportional_millionths: 0,
			},
			cltv_expiry_delta: (8 << 8) | 1,
			htlc_minimum_msat: None,
			htlc_maximum_msat: None,
		};

		let mut invalid_last_hops = last_hops(&nodes);
		invalid_last_hops.push(invalid_last_hop);
		{
			if let Err(LightningError{err, action: ErrorAction::IgnoreError}) = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[6], None, None, &invalid_last_hops.iter().collect::<Vec<_>>(), 100, 42, Arc::clone(&logger)) {
				assert_eq!(err, "Last hop cannot have a payee as a source.");
			} else { panic!(); }
		}

		let route = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[6], None, None, &last_hops(&nodes).iter().collect::<Vec<_>>(), 100, 42, Arc::clone(&logger)).unwrap();
		assert_eq!(route.paths[0].len(), 5);

		assert_eq!(route.paths[0][0].pubkey, nodes[1]);
		assert_eq!(route.paths[0][0].short_channel_id, 2);
		assert_eq!(route.paths[0][0].fee_msat, 100);
		assert_eq!(route.paths[0][0].cltv_expiry_delta, (4 << 8) | 1);
		assert_eq!(route.paths[0][0].node_features.le_flags(), &id_to_feature_flags(2));
		assert_eq!(route.paths[0][0].channel_features.le_flags(), &id_to_feature_flags(2));

		assert_eq!(route.paths[0][1].pubkey, nodes[2]);
		assert_eq!(route.paths[0][1].short_channel_id, 4);
		assert_eq!(route.paths[0][1].fee_msat, 0);
		assert_eq!(route.paths[0][1].cltv_expiry_delta, (6 << 8) | 1);
		assert_eq!(route.paths[0][1].node_features.le_flags(), &id_to_feature_flags(3));
		assert_eq!(route.paths[0][1].channel_features.le_flags(), &id_to_feature_flags(4));

		assert_eq!(route.paths[0][2].pubkey, nodes[4]);
		assert_eq!(route.paths[0][2].short_channel_id, 6);
		assert_eq!(route.paths[0][2].fee_msat, 0);
		assert_eq!(route.paths[0][2].cltv_expiry_delta, (11 << 8) | 1);
		assert_eq!(route.paths[0][2].node_features.le_flags(), &id_to_feature_flags(5));
		assert_eq!(route.paths[0][2].channel_features.le_flags(), &id_to_feature_flags(6));

		assert_eq!(route.paths[0][3].pubkey, nodes[3]);
		assert_eq!(route.paths[0][3].short_channel_id, 11);
		assert_eq!(route.paths[0][3].fee_msat, 0);
		assert_eq!(route.paths[0][3].cltv_expiry_delta, (8 << 8) | 1);
		// If we have a peer in the node map, we'll use their features here since we don't have
		// a way of figuring out their features from the invoice:
		assert_eq!(route.paths[0][3].node_features.le_flags(), &id_to_feature_flags(4));
		assert_eq!(route.paths[0][3].channel_features.le_flags(), &id_to_feature_flags(11));

		assert_eq!(route.paths[0][4].pubkey, nodes[6]);
		assert_eq!(route.paths[0][4].short_channel_id, 8);
		assert_eq!(route.paths[0][4].fee_msat, 100);
		assert_eq!(route.paths[0][4].cltv_expiry_delta, 42);
		assert_eq!(route.paths[0][4].node_features.le_flags(), &Vec::<u8>::new()); // We dont pass flags in from invoices yet
		assert_eq!(route.paths[0][4].channel_features.le_flags(), &Vec::<u8>::new()); // We can't learn any flags from invoices, sadly
	}

	#[test]
	fn our_chans_last_hop_connect_test() {
		let (secp_ctx, net_graph_msg_handler, _, logger) = build_graph();
		let (_, our_id, _, nodes) = get_nodes(&secp_ctx);

		// Simple test with outbound channel to 4 to test that last_hops and first_hops connect
		let our_chans = vec![channelmanager::ChannelDetails {
			channel_id: [0; 32],
			short_channel_id: Some(42),
			remote_network_id: nodes[3].clone(),
			counterparty_features: InitFeatures::from_le_bytes(vec![0b11]),
			channel_value_satoshis: 0,
			user_id: 0,
			outbound_capacity_msat: 250_000_000,
			inbound_capacity_msat: 0,
			is_live: true,
			counterparty_forwarding_info: None,
		}];
		let mut last_hops = last_hops(&nodes);
		let route = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[6], None, Some(&our_chans.iter().collect::<Vec<_>>()), &last_hops.iter().collect::<Vec<_>>(), 100, 42, Arc::clone(&logger)).unwrap();
		assert_eq!(route.paths[0].len(), 2);

		assert_eq!(route.paths[0][0].pubkey, nodes[3]);
		assert_eq!(route.paths[0][0].short_channel_id, 42);
		assert_eq!(route.paths[0][0].fee_msat, 0);
		assert_eq!(route.paths[0][0].cltv_expiry_delta, (8 << 8) | 1);
		assert_eq!(route.paths[0][0].node_features.le_flags(), &vec![0b11]);
		assert_eq!(route.paths[0][0].channel_features.le_flags(), &Vec::<u8>::new()); // No feature flags will meet the relevant-to-channel conversion

		assert_eq!(route.paths[0][1].pubkey, nodes[6]);
		assert_eq!(route.paths[0][1].short_channel_id, 8);
		assert_eq!(route.paths[0][1].fee_msat, 100);
		assert_eq!(route.paths[0][1].cltv_expiry_delta, 42);
		assert_eq!(route.paths[0][1].node_features.le_flags(), &Vec::<u8>::new()); // We dont pass flags in from invoices yet
		assert_eq!(route.paths[0][1].channel_features.le_flags(), &Vec::<u8>::new()); // We can't learn any flags from invoices, sadly

		last_hops[0].fees.base_msat = 1000;

		// Revert to via 6 as the fee on 8 goes up
		let route = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[6], None, None, &last_hops.iter().collect::<Vec<_>>(), 100, 42, Arc::clone(&logger)).unwrap();
		assert_eq!(route.paths[0].len(), 4);

		assert_eq!(route.paths[0][0].pubkey, nodes[1]);
		assert_eq!(route.paths[0][0].short_channel_id, 2);
		assert_eq!(route.paths[0][0].fee_msat, 200); // fee increased as its % of value transferred across node
		assert_eq!(route.paths[0][0].cltv_expiry_delta, (4 << 8) | 1);
		assert_eq!(route.paths[0][0].node_features.le_flags(), &id_to_feature_flags(2));
		assert_eq!(route.paths[0][0].channel_features.le_flags(), &id_to_feature_flags(2));

		assert_eq!(route.paths[0][1].pubkey, nodes[2]);
		assert_eq!(route.paths[0][1].short_channel_id, 4);
		assert_eq!(route.paths[0][1].fee_msat, 100);
		assert_eq!(route.paths[0][1].cltv_expiry_delta, (7 << 8) | 1);
		assert_eq!(route.paths[0][1].node_features.le_flags(), &id_to_feature_flags(3));
		assert_eq!(route.paths[0][1].channel_features.le_flags(), &id_to_feature_flags(4));

		assert_eq!(route.paths[0][2].pubkey, nodes[5]);
		assert_eq!(route.paths[0][2].short_channel_id, 7);
		assert_eq!(route.paths[0][2].fee_msat, 0);
		assert_eq!(route.paths[0][2].cltv_expiry_delta, (10 << 8) | 1);
		// If we have a peer in the node map, we'll use their features here since we don't have
		// a way of figuring out their features from the invoice:
		assert_eq!(route.paths[0][2].node_features.le_flags(), &id_to_feature_flags(6));
		assert_eq!(route.paths[0][2].channel_features.le_flags(), &id_to_feature_flags(7));

		assert_eq!(route.paths[0][3].pubkey, nodes[6]);
		assert_eq!(route.paths[0][3].short_channel_id, 10);
		assert_eq!(route.paths[0][3].fee_msat, 100);
		assert_eq!(route.paths[0][3].cltv_expiry_delta, 42);
		assert_eq!(route.paths[0][3].node_features.le_flags(), &Vec::<u8>::new()); // We dont pass flags in from invoices yet
		assert_eq!(route.paths[0][3].channel_features.le_flags(), &Vec::<u8>::new()); // We can't learn any flags from invoices, sadly

		// ...but still use 8 for larger payments as 6 has a variable feerate
		let route = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[6], None, None, &last_hops.iter().collect::<Vec<_>>(), 2000, 42, Arc::clone(&logger)).unwrap();
		assert_eq!(route.paths[0].len(), 5);

		assert_eq!(route.paths[0][0].pubkey, nodes[1]);
		assert_eq!(route.paths[0][0].short_channel_id, 2);
		assert_eq!(route.paths[0][0].fee_msat, 3000);
		assert_eq!(route.paths[0][0].cltv_expiry_delta, (4 << 8) | 1);
		assert_eq!(route.paths[0][0].node_features.le_flags(), &id_to_feature_flags(2));
		assert_eq!(route.paths[0][0].channel_features.le_flags(), &id_to_feature_flags(2));

		assert_eq!(route.paths[0][1].pubkey, nodes[2]);
		assert_eq!(route.paths[0][1].short_channel_id, 4);
		assert_eq!(route.paths[0][1].fee_msat, 0);
		assert_eq!(route.paths[0][1].cltv_expiry_delta, (6 << 8) | 1);
		assert_eq!(route.paths[0][1].node_features.le_flags(), &id_to_feature_flags(3));
		assert_eq!(route.paths[0][1].channel_features.le_flags(), &id_to_feature_flags(4));

		assert_eq!(route.paths[0][2].pubkey, nodes[4]);
		assert_eq!(route.paths[0][2].short_channel_id, 6);
		assert_eq!(route.paths[0][2].fee_msat, 0);
		assert_eq!(route.paths[0][2].cltv_expiry_delta, (11 << 8) | 1);
		assert_eq!(route.paths[0][2].node_features.le_flags(), &id_to_feature_flags(5));
		assert_eq!(route.paths[0][2].channel_features.le_flags(), &id_to_feature_flags(6));

		assert_eq!(route.paths[0][3].pubkey, nodes[3]);
		assert_eq!(route.paths[0][3].short_channel_id, 11);
		assert_eq!(route.paths[0][3].fee_msat, 1000);
		assert_eq!(route.paths[0][3].cltv_expiry_delta, (8 << 8) | 1);
		// If we have a peer in the node map, we'll use their features here since we don't have
		// a way of figuring out their features from the invoice:
		assert_eq!(route.paths[0][3].node_features.le_flags(), &id_to_feature_flags(4));
		assert_eq!(route.paths[0][3].channel_features.le_flags(), &id_to_feature_flags(11));

		assert_eq!(route.paths[0][4].pubkey, nodes[6]);
		assert_eq!(route.paths[0][4].short_channel_id, 8);
		assert_eq!(route.paths[0][4].fee_msat, 2000);
		assert_eq!(route.paths[0][4].cltv_expiry_delta, 42);
		assert_eq!(route.paths[0][4].node_features.le_flags(), &Vec::<u8>::new()); // We dont pass flags in from invoices yet
		assert_eq!(route.paths[0][4].channel_features.le_flags(), &Vec::<u8>::new()); // We can't learn any flags from invoices, sadly
	}

	#[test]
	fn unannounced_path_test() {
		// We should be able to send a payment to a destination without any help of a routing graph
		// if we have a channel with a common counterparty that appears in the first and last hop
		// hints.
		let source_node_id = PublicKey::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&hex::decode(format!("{:02}", 41).repeat(32)).unwrap()[..]).unwrap());
		let middle_node_id = PublicKey::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&hex::decode(format!("{:02}", 42).repeat(32)).unwrap()[..]).unwrap());
		let target_node_id = PublicKey::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&hex::decode(format!("{:02}", 43).repeat(32)).unwrap()[..]).unwrap());

		// If we specify a channel to a middle hop, that overrides our local channel view and that gets used
		let last_hops = vec![RouteHint {
			src_node_id: middle_node_id,
			short_channel_id: 8,
			fees: RoutingFees {
				base_msat: 1000,
				proportional_millionths: 0,
			},
			cltv_expiry_delta: (8 << 8) | 1,
			htlc_minimum_msat: None,
			htlc_maximum_msat: None,
		}];
		let our_chans = vec![channelmanager::ChannelDetails {
			channel_id: [0; 32],
			short_channel_id: Some(42),
			remote_network_id: middle_node_id,
			counterparty_features: InitFeatures::from_le_bytes(vec![0b11]),
			channel_value_satoshis: 100000,
			user_id: 0,
			outbound_capacity_msat: 100000,
			inbound_capacity_msat: 100000,
			is_live: true,
			counterparty_forwarding_info: None,
		}];
		let route = get_route(&source_node_id, &NetworkGraph::new(genesis_block(Network::Testnet).header.block_hash()), &target_node_id, None, Some(&our_chans.iter().collect::<Vec<_>>()), &last_hops.iter().collect::<Vec<_>>(), 100, 42, Arc::new(test_utils::TestLogger::new())).unwrap();

		assert_eq!(route.paths[0].len(), 2);

		assert_eq!(route.paths[0][0].pubkey, middle_node_id);
		assert_eq!(route.paths[0][0].short_channel_id, 42);
		assert_eq!(route.paths[0][0].fee_msat, 1000);
		assert_eq!(route.paths[0][0].cltv_expiry_delta, (8 << 8) | 1);
		assert_eq!(route.paths[0][0].node_features.le_flags(), &[0b11]);
		assert_eq!(route.paths[0][0].channel_features.le_flags(), &[0; 0]); // We can't learn any flags from invoices, sadly

		assert_eq!(route.paths[0][1].pubkey, target_node_id);
		assert_eq!(route.paths[0][1].short_channel_id, 8);
		assert_eq!(route.paths[0][1].fee_msat, 100);
		assert_eq!(route.paths[0][1].cltv_expiry_delta, 42);
		assert_eq!(route.paths[0][1].node_features.le_flags(), &[0; 0]); // We dont pass flags in from invoices yet
		assert_eq!(route.paths[0][1].channel_features.le_flags(), &[0; 0]); // We can't learn any flags from invoices, sadly
	}

	#[test]
	fn available_amount_while_routing_test() {
		// Tests whether we choose the correct available channel amount while routing.

		let (secp_ctx, mut net_graph_msg_handler, chain_monitor, logger) = build_graph();
		let (our_privkey, our_id, privkeys, nodes) = get_nodes(&secp_ctx);

		// We will use a simple single-path route from
		// our node to node2 via node0: channels {1, 3}.

		// First disable all other paths.
		update_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 2,
			timestamp: 2,
			flags: 2,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(100_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 12,
			timestamp: 2,
			flags: 2,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(100_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		// Make the first channel (#1) very permissive,
		// and we will be testing all limits on the second channel.
		update_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 1,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(1_000_000_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		// First, let's see if routing works if we have absolutely no idea about the available amount.
		// In this case, it should be set to 250_000 sats.
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[0], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 3,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		{
			// Attempt to route more than available results in a failure.
			if let Err(LightningError{err, action: ErrorAction::IgnoreError}) = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[2],
					Some(InvoiceFeatures::known()), None, &Vec::new(), 250_000_001, 42, Arc::clone(&logger)) {
				assert_eq!(err, "Failed to find a sufficient route to the given destination");
			} else { panic!(); }
		}

		{
			// Now, attempt to route an exact amount we have should be fine.
			let route = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[2],
				Some(InvoiceFeatures::known()), None, &Vec::new(), 250_000_000, 42, Arc::clone(&logger)).unwrap();
			assert_eq!(route.paths.len(), 1);
			let path = route.paths.last().unwrap();
			assert_eq!(path.len(), 2);
			assert_eq!(path.last().unwrap().pubkey, nodes[2]);
			assert_eq!(path.last().unwrap().fee_msat, 250_000_000);
		}

		// Check that setting outbound_capacity_msat in first_hops limits the channels.
		// Disable channel #1 and use another first hop.
		update_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 1,
			timestamp: 3,
			flags: 2,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(1_000_000_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		// Now, limit the first_hop by the outbound_capacity_msat of 200_000 sats.
		let our_chans = vec![channelmanager::ChannelDetails {
			channel_id: [0; 32],
			short_channel_id: Some(42),
			remote_network_id: nodes[0].clone(),
			counterparty_features: InitFeatures::from_le_bytes(vec![0b11]),
			channel_value_satoshis: 0,
			user_id: 0,
			outbound_capacity_msat: 200_000_000,
			inbound_capacity_msat: 0,
			is_live: true,
			counterparty_forwarding_info: None,
		}];

		{
			// Attempt to route more than available results in a failure.
			if let Err(LightningError{err, action: ErrorAction::IgnoreError}) = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[2],
					Some(InvoiceFeatures::known()), Some(&our_chans.iter().collect::<Vec<_>>()), &Vec::new(), 200_000_001, 42, Arc::clone(&logger)) {
				assert_eq!(err, "Failed to find a sufficient route to the given destination");
			} else { panic!(); }
		}

		{
			// Now, attempt to route an exact amount we have should be fine.
			let route = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[2],
				Some(InvoiceFeatures::known()), Some(&our_chans.iter().collect::<Vec<_>>()), &Vec::new(), 200_000_000, 42, Arc::clone(&logger)).unwrap();
			assert_eq!(route.paths.len(), 1);
			let path = route.paths.last().unwrap();
			assert_eq!(path.len(), 2);
			assert_eq!(path.last().unwrap().pubkey, nodes[2]);
			assert_eq!(path.last().unwrap().fee_msat, 200_000_000);
		}

		// Enable channel #1 back.
		update_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 1,
			timestamp: 4,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(1_000_000_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});


		// Now let's see if routing works if we know only htlc_maximum_msat.
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[0], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 3,
			timestamp: 3,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(15_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		{
			// Attempt to route more than available results in a failure.
			if let Err(LightningError{err, action: ErrorAction::IgnoreError}) = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[2],
					Some(InvoiceFeatures::known()), None, &Vec::new(), 15_001, 42, Arc::clone(&logger)) {
				assert_eq!(err, "Failed to find a sufficient route to the given destination");
			} else { panic!(); }
		}

		{
			// Now, attempt to route an exact amount we have should be fine.
			let route = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[2],
				Some(InvoiceFeatures::known()), None, &Vec::new(), 15_000, 42, Arc::clone(&logger)).unwrap();
			assert_eq!(route.paths.len(), 1);
			let path = route.paths.last().unwrap();
			assert_eq!(path.len(), 2);
			assert_eq!(path.last().unwrap().pubkey, nodes[2]);
			assert_eq!(path.last().unwrap().fee_msat, 15_000);
		}

		// Now let's see if routing works if we know only capacity from the UTXO.

		// We can't change UTXO capacity on the fly, so we'll disable
		// the existing channel and add another one with the capacity we need.
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[0], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 3,
			timestamp: 4,
			flags: 2,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		let good_script = Builder::new().push_opcode(opcodes::all::OP_PUSHNUM_2)
		.push_slice(&PublicKey::from_secret_key(&secp_ctx, &privkeys[0]).serialize())
		.push_slice(&PublicKey::from_secret_key(&secp_ctx, &privkeys[2]).serialize())
		.push_opcode(opcodes::all::OP_PUSHNUM_2)
		.push_opcode(opcodes::all::OP_CHECKMULTISIG).into_script().to_v0_p2wsh();

		*chain_monitor.utxo_ret.lock().unwrap() = Ok(TxOut { value: 15, script_pubkey: good_script.clone() });
		net_graph_msg_handler.add_chain_access(Some(chain_monitor));

		add_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[0], &privkeys[2], ChannelFeatures::from_le_bytes(id_to_feature_flags(3)), 333);
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[0], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 333,
			timestamp: 1,
			flags: 0,
			cltv_expiry_delta: (3 << 8) | 1,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[2], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 333,
			timestamp: 1,
			flags: 1,
			cltv_expiry_delta: (3 << 8) | 2,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 100,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		{
			// Attempt to route more than available results in a failure.
			if let Err(LightningError{err, action: ErrorAction::IgnoreError}) = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[2],
					Some(InvoiceFeatures::known()), None, &Vec::new(), 15_001, 42, Arc::clone(&logger)) {
				assert_eq!(err, "Failed to find a sufficient route to the given destination");
			} else { panic!(); }
		}

		{
			// Now, attempt to route an exact amount we have should be fine.
			let route = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[2],
				Some(InvoiceFeatures::known()), None, &Vec::new(), 15_000, 42, Arc::clone(&logger)).unwrap();
			assert_eq!(route.paths.len(), 1);
			let path = route.paths.last().unwrap();
			assert_eq!(path.len(), 2);
			assert_eq!(path.last().unwrap().pubkey, nodes[2]);
			assert_eq!(path.last().unwrap().fee_msat, 15_000);
		}

		// Now let's see if routing chooses htlc_maximum_msat over UTXO capacity.
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[0], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 333,
			timestamp: 6,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(10_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		{
			// Attempt to route more than available results in a failure.
			if let Err(LightningError{err, action: ErrorAction::IgnoreError}) = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[2],
					Some(InvoiceFeatures::known()), None, &Vec::new(), 10_001, 42, Arc::clone(&logger)) {
				assert_eq!(err, "Failed to find a sufficient route to the given destination");
			} else { panic!(); }
		}

		{
			// Now, attempt to route an exact amount we have should be fine.
			let route = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[2],
				Some(InvoiceFeatures::known()), None, &Vec::new(), 10_000, 42, Arc::clone(&logger)).unwrap();
			assert_eq!(route.paths.len(), 1);
			let path = route.paths.last().unwrap();
			assert_eq!(path.len(), 2);
			assert_eq!(path.last().unwrap().pubkey, nodes[2]);
			assert_eq!(path.last().unwrap().fee_msat, 10_000);
		}
	}

	#[test]
	fn available_liquidity_last_hop_test() {
		// Check that available liquidity properly limits the path even when only
		// one of the latter hops is limited.
		let (secp_ctx, net_graph_msg_handler, _, logger) = build_graph();
		let (our_privkey, our_id, privkeys, nodes) = get_nodes(&secp_ctx);

		// Path via {node7, node2, node4} is channels {12, 13, 6, 11}.
		// {12, 13, 11} have the capacities of 100, {6} has a capacity of 50.
		// Total capacity: 50 sats.

		// Disable other potential paths.
		update_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 2,
			timestamp: 2,
			flags: 2,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(100_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[2], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 7,
			timestamp: 2,
			flags: 2,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(100_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		// Limit capacities

		update_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 12,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(100_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[7], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 13,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(100_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[2], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 6,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(50_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[4], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 11,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(100_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		{
			// Attempt to route more than available results in a failure.
			if let Err(LightningError{err, action: ErrorAction::IgnoreError}) = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[3],
					Some(InvoiceFeatures::known()), None, &Vec::new(), 60_000, 42, Arc::clone(&logger)) {
				assert_eq!(err, "Failed to find a sufficient route to the given destination");
			} else { panic!(); }
		}

		{
			// Now, attempt to route 49 sats (just a bit below the capacity).
			let route = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[3],
				Some(InvoiceFeatures::known()), None, &Vec::new(), 49_000, 42, Arc::clone(&logger)).unwrap();
			assert_eq!(route.paths.len(), 1);
			let mut total_amount_paid_msat = 0;
			for path in &route.paths {
				assert_eq!(path.len(), 4);
				assert_eq!(path.last().unwrap().pubkey, nodes[3]);
				total_amount_paid_msat += path.last().unwrap().fee_msat;
			}
			assert_eq!(total_amount_paid_msat, 49_000);
		}

		{
			// Attempt to route an exact amount is also fine
			let route = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[3],
				Some(InvoiceFeatures::known()), None, &Vec::new(), 50_000, 42, Arc::clone(&logger)).unwrap();
			assert_eq!(route.paths.len(), 1);
			let mut total_amount_paid_msat = 0;
			for path in &route.paths {
				assert_eq!(path.len(), 4);
				assert_eq!(path.last().unwrap().pubkey, nodes[3]);
				total_amount_paid_msat += path.last().unwrap().fee_msat;
			}
			assert_eq!(total_amount_paid_msat, 50_000);
		}
	}

	#[test]
	fn ignore_fee_first_hop_test() {
		let (secp_ctx, net_graph_msg_handler, _, logger) = build_graph();
		let (our_privkey, our_id, privkeys, nodes) = get_nodes(&secp_ctx);

		// Path via node0 is channels {1, 3}. Limit them to 100 and 50 sats (total limit 50).
		update_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 1,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(100_000),
			fee_base_msat: 1_000_000,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[0], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 3,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(50_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		{
			let route = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[2], None, None, &Vec::new(), 50_000, 42, Arc::clone(&logger)).unwrap();
			assert_eq!(route.paths.len(), 1);
			let mut total_amount_paid_msat = 0;
			for path in &route.paths {
				assert_eq!(path.len(), 2);
				assert_eq!(path.last().unwrap().pubkey, nodes[2]);
				total_amount_paid_msat += path.last().unwrap().fee_msat;
			}
			assert_eq!(total_amount_paid_msat, 50_000);
		}
	}

	#[test]
	fn simple_mpp_route_test() {
		let (secp_ctx, net_graph_msg_handler, _, logger) = build_graph();
		let (our_privkey, our_id, privkeys, nodes) = get_nodes(&secp_ctx);

		// We need a route consisting of 3 paths:
		// From our node to node2 via node0, node7, node1 (three paths one hop each).
		// To achieve this, the amount being transferred should be around
		// the total capacity of these 3 paths.

		// First, we set limits on these (previously unlimited) channels.
		// Their aggregate capacity will be 50 + 60 + 180 = 290 sats.

		// Path via node0 is channels {1, 3}. Limit them to 100 and 50 sats (total limit 50).
		update_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 1,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(100_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[0], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 3,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(50_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		// Path via node7 is channels {12, 13}. Limit them to 60 and 60 sats
		// (total limit 60).
		update_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 12,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(60_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[7], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 13,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(60_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		// Path via node1 is channels {2, 4}. Limit them to 200 and 180 sats
		// (total capacity 180 sats).
		update_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 2,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(200_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[1], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 4,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(180_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		{
			// Attempt to route more than available results in a failure.
			if let Err(LightningError{err, action: ErrorAction::IgnoreError}) = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(),
					&nodes[2], Some(InvoiceFeatures::known()), None, &Vec::new(), 300_000, 42, Arc::clone(&logger)) {
				assert_eq!(err, "Failed to find a sufficient route to the given destination");
			} else { panic!(); }
		}

		{
			// Now, attempt to route 250 sats (just a bit below the capacity).
			// Our algorithm should provide us with these 3 paths.
			let route = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[2],
				Some(InvoiceFeatures::known()), None, &Vec::new(), 250_000, 42, Arc::clone(&logger)).unwrap();
			assert_eq!(route.paths.len(), 3);
			let mut total_amount_paid_msat = 0;
			for path in &route.paths {
				assert_eq!(path.len(), 2);
				assert_eq!(path.last().unwrap().pubkey, nodes[2]);
				total_amount_paid_msat += path.last().unwrap().fee_msat;
			}
			assert_eq!(total_amount_paid_msat, 250_000);
		}

		{
			// Attempt to route an exact amount is also fine
			let route = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[2],
				Some(InvoiceFeatures::known()), None, &Vec::new(), 290_000, 42, Arc::clone(&logger)).unwrap();
			assert_eq!(route.paths.len(), 3);
			let mut total_amount_paid_msat = 0;
			for path in &route.paths {
				assert_eq!(path.len(), 2);
				assert_eq!(path.last().unwrap().pubkey, nodes[2]);
				total_amount_paid_msat += path.last().unwrap().fee_msat;
			}
			assert_eq!(total_amount_paid_msat, 290_000);
		}
	}

	#[test]
	fn long_mpp_route_test() {
		let (secp_ctx, net_graph_msg_handler, _, logger) = build_graph();
		let (our_privkey, our_id, privkeys, nodes) = get_nodes(&secp_ctx);

		// We need a route consisting of 3 paths:
		// From our node to node3 via {node0, node2}, {node7, node2, node4} and {node7, node2}.
		// Note that these paths overlap (channels 5, 12, 13).
		// We will route 300 sats.
		// Each path will have 100 sats capacity, those channels which
		// are used twice will have 200 sats capacity.

		// Disable other potential paths.
		update_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 2,
			timestamp: 2,
			flags: 2,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(100_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[2], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 7,
			timestamp: 2,
			flags: 2,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(100_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		// Path via {node0, node2} is channels {1, 3, 5}.
		update_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 1,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(100_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[0], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 3,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(100_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		// Capacity of 200 sats because this channel will be used by 3rd path as well.
		add_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[2], &privkeys[3], ChannelFeatures::from_le_bytes(id_to_feature_flags(5)), 5);
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[2], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 5,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(200_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		// Path via {node7, node2, node4} is channels {12, 13, 6, 11}.
		// Add 100 sats to the capacities of {12, 13}, because these channels
		// are also used for 3rd path. 100 sats for the rest. Total capacity: 100 sats.
		update_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 12,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(200_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[7], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 13,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(200_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[2], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 6,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(100_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[4], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 11,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(100_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		// Path via {node7, node2} is channels {12, 13, 5}.
		// We already limited them to 200 sats (they are used twice for 100 sats).
		// Nothing to do here.

		{
			// Attempt to route more than available results in a failure.
			if let Err(LightningError{err, action: ErrorAction::IgnoreError}) = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[3],
					Some(InvoiceFeatures::known()), None, &Vec::new(), 350_000, 42, Arc::clone(&logger)) {
				assert_eq!(err, "Failed to find a sufficient route to the given destination");
			} else { panic!(); }
		}

		{
			// Now, attempt to route 300 sats (exact amount we can route).
			// Our algorithm should provide us with these 3 paths, 100 sats each.
			let route = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[3],
				Some(InvoiceFeatures::known()), None, &Vec::new(), 300_000, 42, Arc::clone(&logger)).unwrap();
			assert_eq!(route.paths.len(), 3);

			let mut total_amount_paid_msat = 0;
			for path in &route.paths {
				assert_eq!(path.last().unwrap().pubkey, nodes[3]);
				total_amount_paid_msat += path.last().unwrap().fee_msat;
			}
			assert_eq!(total_amount_paid_msat, 300_000);
		}

	}

	#[test]
	fn mpp_cheaper_route_test() {
		let (secp_ctx, net_graph_msg_handler, _, logger) = build_graph();
		let (our_privkey, our_id, privkeys, nodes) = get_nodes(&secp_ctx);

		// This test checks that if we have two cheaper paths and one more expensive path,
		// so that liquidity-wise any 2 of 3 combination is sufficient,
		// two cheaper paths will be taken.
		// These paths have equal available liquidity.

		// We need a combination of 3 paths:
		// From our node to node3 via {node0, node2}, {node7, node2, node4} and {node7, node2}.
		// Note that these paths overlap (channels 5, 12, 13).
		// Each path will have 100 sats capacity, those channels which
		// are used twice will have 200 sats capacity.

		// Disable other potential paths.
		update_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 2,
			timestamp: 2,
			flags: 2,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(100_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[2], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 7,
			timestamp: 2,
			flags: 2,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(100_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		// Path via {node0, node2} is channels {1, 3, 5}.
		update_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 1,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(100_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[0], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 3,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(100_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		// Capacity of 200 sats because this channel will be used by 3rd path as well.
		add_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[2], &privkeys[3], ChannelFeatures::from_le_bytes(id_to_feature_flags(5)), 5);
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[2], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 5,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(200_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		// Path via {node7, node2, node4} is channels {12, 13, 6, 11}.
		// Add 100 sats to the capacities of {12, 13}, because these channels
		// are also used for 3rd path. 100 sats for the rest. Total capacity: 100 sats.
		update_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 12,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(200_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[7], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 13,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(200_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[2], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 6,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(100_000),
			fee_base_msat: 1_000,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[4], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 11,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(100_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		// Path via {node7, node2} is channels {12, 13, 5}.
		// We already limited them to 200 sats (they are used twice for 100 sats).
		// Nothing to do here.

		{
			// Now, attempt to route 180 sats.
			// Our algorithm should provide us with these 2 paths.
			let route = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[3],
				Some(InvoiceFeatures::known()), None, &Vec::new(), 180_000, 42, Arc::clone(&logger)).unwrap();
			assert_eq!(route.paths.len(), 2);

			let mut total_value_transferred_msat = 0;
			let mut total_paid_msat = 0;
			for path in &route.paths {
				assert_eq!(path.last().unwrap().pubkey, nodes[3]);
				total_value_transferred_msat += path.last().unwrap().fee_msat;
				for hop in path {
					total_paid_msat += hop.fee_msat;
				}
			}
			// If we paid fee, this would be higher.
			assert_eq!(total_value_transferred_msat, 180_000);
			let total_fees_paid = total_paid_msat - total_value_transferred_msat;
			assert_eq!(total_fees_paid, 0);
		}
	}

	#[test]
	fn fees_on_mpp_route_test() {
		// This test makes sure that MPP algorithm properly takes into account
		// fees charged on the channels, by making the fees impactful:
		// if the fee is not properly accounted for, the behavior is different.
		let (secp_ctx, net_graph_msg_handler, _, logger) = build_graph();
		let (our_privkey, our_id, privkeys, nodes) = get_nodes(&secp_ctx);

		// We need a route consisting of 2 paths:
		// From our node to node3 via {node0, node2} and {node7, node2, node4}.
		// We will route 200 sats, Each path will have 100 sats capacity.

		// This test is not particularly stable: e.g.,
		// there's a way to route via {node0, node2, node4}.
		// It works while pathfinding is deterministic, but can be broken otherwise.
		// It's fine to ignore this concern for now.

		// Disable other potential paths.
		update_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 2,
			timestamp: 2,
			flags: 2,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(100_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[2], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 7,
			timestamp: 2,
			flags: 2,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(100_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		// Path via {node0, node2} is channels {1, 3, 5}.
		update_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 1,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(100_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[0], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 3,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(100_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		add_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[2], &privkeys[3], ChannelFeatures::from_le_bytes(id_to_feature_flags(5)), 5);
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[2], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 5,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(100_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		// Path via {node7, node2, node4} is channels {12, 13, 6, 11}.
		// All channels should be 100 sats capacity. But for the fee experiment,
		// we'll add absolute fee of 150 sats paid for the use channel 6 (paid to node2 on channel 13).
		// Since channel 12 allows to deliver only 250 sats to channel 13, channel 13 can transfer only
		// 100 sats (and pay 150 sats in fees for the use of channel 6),
		// so no matter how large are other channels,
		// the whole path will be limited by 100 sats with just these 2 conditions:
		// - channel 12 capacity is 250 sats
		// - fee for channel 6 is 150 sats
		// Let's test this by enforcing these 2 conditions and removing other limits.
		update_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 12,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(250_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[7], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 13,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[2], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 6,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 150_000,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[4], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 11,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Absent,
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		{
			// Attempt to route more than available results in a failure.
			if let Err(LightningError{err, action: ErrorAction::IgnoreError}) = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[3],
					Some(InvoiceFeatures::known()), None, &Vec::new(), 210_000, 42, Arc::clone(&logger)) {
				assert_eq!(err, "Failed to find a sufficient route to the given destination");
			} else { panic!(); }
		}

		{
			// Now, attempt to route 200 sats (exact amount we can route).
			let route = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[3],
				Some(InvoiceFeatures::known()), None, &Vec::new(), 200_000, 42, Arc::clone(&logger)).unwrap();
			assert_eq!(route.paths.len(), 2);

			let mut total_amount_paid_msat = 0;
			for path in &route.paths {
				assert_eq!(path.last().unwrap().pubkey, nodes[3]);
				total_amount_paid_msat += path.last().unwrap().fee_msat;
			}
			assert_eq!(total_amount_paid_msat, 200_000);
		}

	}

	#[test]
	fn drop_lowest_channel_mpp_route_test() {
		// This test checks that low-capacity channel is dropped when after
		// path finding we realize that we found more capacity than we need.
		let (secp_ctx, net_graph_msg_handler, _, logger) = build_graph();
		let (our_privkey, our_id, privkeys, nodes) = get_nodes(&secp_ctx);

		// We need a route consisting of 3 paths:
		// From our node to node2 via node0, node7, node1 (three paths one hop each).

		// The first and the second paths should be sufficient, but the third should be
		// cheaper, so that we select it but drop later.

		// First, we set limits on these (previously unlimited) channels.
		// Their aggregate capacity will be 50 + 60 + 20 = 130 sats.

		// Path via node0 is channels {1, 3}. Limit them to 100 and 50 sats (total limit 50);
		update_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 1,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(100_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[0], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 3,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(50_000),
			fee_base_msat: 100,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		// Path via node7 is channels {12, 13}. Limit them to 60 and 60 sats (total limit 60);
		update_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 12,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(60_000),
			fee_base_msat: 100,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[7], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 13,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(60_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		// Path via node1 is channels {2, 4}. Limit them to 20 and 20 sats (total capacity 20 sats).
		update_channel(&net_graph_msg_handler, &secp_ctx, &our_privkey, UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 2,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(20_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});
		update_channel(&net_graph_msg_handler, &secp_ctx, &privkeys[1], UnsignedChannelUpdate {
			chain_hash: genesis_block(Network::Testnet).header.block_hash(),
			short_channel_id: 4,
			timestamp: 2,
			flags: 0,
			cltv_expiry_delta: 0,
			htlc_minimum_msat: 0,
			htlc_maximum_msat: OptionalField::Present(20_000),
			fee_base_msat: 0,
			fee_proportional_millionths: 0,
			excess_data: Vec::new()
		});

		{
			// Attempt to route more than available results in a failure.
			if let Err(LightningError{err, action: ErrorAction::IgnoreError}) = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[2],
					Some(InvoiceFeatures::known()), None, &Vec::new(), 150_000, 42, Arc::clone(&logger)) {
				assert_eq!(err, "Failed to find a sufficient route to the given destination");
			} else { panic!(); }
		}

		{
			// Now, attempt to route 125 sats (just a bit below the capacity of 3 channels).
			// Our algorithm should provide us with these 3 paths.
			let route = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[2],
				Some(InvoiceFeatures::known()), None, &Vec::new(), 125_000, 42, Arc::clone(&logger)).unwrap();
			assert_eq!(route.paths.len(), 3);
			let mut total_amount_paid_msat = 0;
			for path in &route.paths {
				assert_eq!(path.len(), 2);
				assert_eq!(path.last().unwrap().pubkey, nodes[2]);
				total_amount_paid_msat += path.last().unwrap().fee_msat;
			}
			assert_eq!(total_amount_paid_msat, 125_000);
		}

		{
			// Attempt to route without the last small cheap channel
			let route = get_route(&our_id, &net_graph_msg_handler.network_graph.read().unwrap(), &nodes[2],
				Some(InvoiceFeatures::known()), None, &Vec::new(), 90_000, 42, Arc::clone(&logger)).unwrap();
			assert_eq!(route.paths.len(), 2);
			let mut total_amount_paid_msat = 0;
			for path in &route.paths {
				assert_eq!(path.len(), 2);
				assert_eq!(path.last().unwrap().pubkey, nodes[2]);
				total_amount_paid_msat += path.last().unwrap().fee_msat;
			}
			assert_eq!(total_amount_paid_msat, 90_000);
		}
	}
}

#[cfg(all(test, feature = "unstable"))]
mod benches {
	use super::*;
	use util::logger::{Logger, Record};

	use std::fs::File;
	use test::Bencher;

	struct DummyLogger {}
	impl Logger for DummyLogger {
		fn log(&self, _record: &Record) {}
	}

	#[bench]
	fn generate_routes(bench: &mut Bencher) {
		let mut d = File::open("net_graph-2021-02-12.bin").expect("Please fetch https://bitcoin.ninja/ldk-net_graph-879e309c128-2020-02-12.bin and place it at lightning/net_graph-2021-02-12.bin");
		let graph = NetworkGraph::read(&mut d).unwrap();

		// First, get 100 (source, destination) pairs for which route-getting actually succeeds...
		let mut path_endpoints = Vec::new();
		let mut seed: usize = 0xdeadbeef;
		'load_endpoints: for _ in 0..100 {
			loop {
				seed *= 0xdeadbeef;
				let src = graph.get_nodes().keys().skip(seed % graph.get_nodes().len()).next().unwrap();
				seed *= 0xdeadbeef;
				let dst = graph.get_nodes().keys().skip(seed % graph.get_nodes().len()).next().unwrap();
				let amt = seed as u64 % 1_000_000;
				if get_route(src, &graph, dst, None, None, &[], amt, 42, &DummyLogger{}).is_ok() {
					path_endpoints.push((src, dst, amt));
					continue 'load_endpoints;
				}
			}
		}

		// ...then benchmark finding paths between the nodes we learned.
		let mut idx = 0;
		bench.iter(|| {
			let (src, dst, amt) = path_endpoints[idx % path_endpoints.len()];
			assert!(get_route(src, &graph, dst, None, None, &[], amt, 42, &DummyLogger{}).is_ok());
			idx += 1;
		});
	}

	#[bench]
	fn generate_mpp_routes(bench: &mut Bencher) {
		let mut d = File::open("net_graph-2021-02-12.bin").expect("Please fetch https://bitcoin.ninja/ldk-net_graph-879e309c128-2020-02-12.bin and place it at lightning/net_graph-2021-02-12.bin");
		let graph = NetworkGraph::read(&mut d).unwrap();

		// First, get 100 (source, destination) pairs for which route-getting actually succeeds...
		let mut path_endpoints = Vec::new();
		let mut seed: usize = 0xdeadbeef;
		'load_endpoints: for _ in 0..100 {
			loop {
				seed *= 0xdeadbeef;
				let src = graph.get_nodes().keys().skip(seed % graph.get_nodes().len()).next().unwrap();
				seed *= 0xdeadbeef;
				let dst = graph.get_nodes().keys().skip(seed % graph.get_nodes().len()).next().unwrap();
				let amt = seed as u64 % 1_000_000;
				if get_route(src, &graph, dst, Some(InvoiceFeatures::known()), None, &[], amt, 42, &DummyLogger{}).is_ok() {
					path_endpoints.push((src, dst, amt));
					continue 'load_endpoints;
				}
			}
		}

		// ...then benchmark finding paths between the nodes we learned.
		let mut idx = 0;
		bench.iter(|| {
			let (src, dst, amt) = path_endpoints[idx % path_endpoints.len()];
			assert!(get_route(src, &graph, dst, Some(InvoiceFeatures::known()), None, &[], amt, 42, &DummyLogger{}).is_ok());
			idx += 1;
		});
	}
}
