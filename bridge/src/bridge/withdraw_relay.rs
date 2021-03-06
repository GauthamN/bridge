use std::sync::Arc;
use futures::{Future, Stream, Poll};
use futures::future::{JoinAll, join_all, Join};
use tokio_timer::Timeout;
use web3::Transport;
use web3::types::{H256, Address, FilterBuilder, Log, Bytes, TransactionRequest};
use ethabi::{RawLog, self};
use app::App;
use api::{self, LogStream, ApiCall};
use contracts::{mainnet, testnet};
use util::web3_filter;
use database::Database;
use error::{self, Error};

fn collected_signatures_filter(testnet: &testnet::KovanBridge, address: Address) -> FilterBuilder {
	let filter = testnet.events().collected_signatures().create_filter();
	web3_filter(filter, address)
}

#[derive(Debug, PartialEq)]
struct RelayAssignment {
	signature_payloads: Vec<Bytes>,
	message_payload: Bytes,
}

fn signatures_payload(testnet: &testnet::KovanBridge, signatures: u32, my_address: Address, log: Log) -> error::Result<Option<RelayAssignment>> {
	let raw_log = RawLog {
		topics: log.topics.into_iter().map(|t| t.0).collect(),
		data: log.data.0,
	};
	let collected_signatures = testnet.events().collected_signatures().parse_log(raw_log)?;
	if collected_signatures.authority != my_address.0 {
		// someone else will relay this transaction to mainnet
		return Ok(None);
	}
	let signature_payloads = (0..signatures).into_iter()
		.map(|index| ethabi::util::pad_u32(index))
		.map(|index| testnet.functions().signature().input(collected_signatures.message_hash, index))
		.map(Into::into)
		.collect();
	let message_payload = testnet.functions().message().input(collected_signatures.message_hash).into();

	Ok(Some(RelayAssignment {
		signature_payloads,
		message_payload,
	}))
}

fn withdraw_relay_payload(mainnet: &mainnet::EthereumBridge, signatures: Vec<Bytes>, message: Bytes) -> Bytes {
	assert_eq!(message.0.len(), 84, "KovanBridge never accepts messages with len != 84 bytes; qed");
	let mut v_vec = Vec::new();
	let mut r_vec = Vec::new();
	let mut s_vec = Vec::new();
	for signature in signatures {
		assert_eq!(signature.0.len(), 65, "KovanBridge never accepts signatures with len != 65 bytes; qed");
		let mut r = [0u8; 32];
		let mut s= [0u8; 32];
		let mut v = [0u8; 32];
		r.copy_from_slice(&signature.0[0..32]);
		s.copy_from_slice(&signature.0[32..64]);
		v[31] = signature.0[64];
		v_vec.push(v);
		s_vec.push(s);
		r_vec.push(r);
	}
	mainnet.functions().withdraw().input(v_vec, r_vec, s_vec, message.0).into()
}

pub enum WithdrawRelayState<T: Transport> {
	Wait,
	Fetch {
		future: Join<JoinAll<Vec<Timeout<ApiCall<Bytes, T::Out>>>>, JoinAll<Vec<JoinAll<Vec<Timeout<ApiCall<Bytes, T::Out>>>>>>>,
		block: u64,
	},
	RelayWithdraws {
		future: JoinAll<Vec<Timeout<ApiCall<H256, T::Out>>>>,
		block: u64,
	},
	Yield(Option<u64>),
}

pub fn create_withdraw_relay<T: Transport + Clone>(app: Arc<App<T>>, init: &Database) -> WithdrawRelay<T> {
	let logs_init = api::LogStreamInit {
		after: init.checked_withdraw_relay,
		request_timeout: app.config.testnet.request_timeout,
		poll_interval: app.config.testnet.poll_interval,
		confirmations: app.config.testnet.required_confirmations,
		filter: collected_signatures_filter(&app.testnet_bridge, init.testnet_contract_address.clone()),
	};

	WithdrawRelay {
		logs: api::log_stream(app.connections.testnet.clone(), app.timer.clone(), logs_init),
		mainnet_contract: init.mainnet_contract_address.clone(),
		testnet_contract: init.testnet_contract_address.clone(),
		state: WithdrawRelayState::Wait,
		app,
	}
}

pub struct WithdrawRelay<T: Transport> {
	app: Arc<App<T>>,
	logs: LogStream<T>,
	state: WithdrawRelayState<T>,
	testnet_contract: Address,
	mainnet_contract: Address,
}

impl<T: Transport> Stream for WithdrawRelay<T> {
	type Item = u64;
	type Error = Error;

	fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
		loop {
			let next_state = match self.state {
				WithdrawRelayState::Wait => {
					let item = try_stream!(self.logs.poll());
					let assignments = item.logs
						.into_iter()
						.map(|log| signatures_payload(
								&self.app.testnet_bridge,
								self.app.config.authorities.required_signatures,
								self.app.config.testnet.account.clone(),
								log))
						.collect::<error::Result<Vec<_>>>()?;

					let (signatures, messages): (Vec<_>, Vec<_>) = assignments.into_iter()
						.filter_map(|a| a)
						.map(|assignment| (assignment.signature_payloads, assignment.message_payload))
						.unzip();

					let message_calls = messages.into_iter()
						.map(|payload| {
							self.app.timer.timeout(
								api::call(&self.app.connections.testnet, self.testnet_contract.clone(), payload),
								self.app.config.testnet.request_timeout)
						})
						.collect::<Vec<_>>();

					let signature_calls = signatures.into_iter()
						.map(|payloads| {
							payloads.into_iter()
								.map(|payload| {
									self.app.timer.timeout(
										api::call(&self.app.connections.testnet, self.testnet_contract.clone(), payload),
										self.app.config.testnet.request_timeout)
								})
								.collect::<Vec<_>>()
						})
						.map(|calls| join_all(calls))
						.collect::<Vec<_>>();

					WithdrawRelayState::Fetch {
						future: join_all(message_calls).join(join_all(signature_calls)),
						block: item.to,
					}
				},
				WithdrawRelayState::Fetch { ref mut future, block } => {
					let (messages, signatures) = try_ready!(future.poll());
					assert_eq!(messages.len(), signatures.len());
					let app = &self.app;
					let mainnet_contract = &self.mainnet_contract;

					let relays = messages.into_iter().zip(signatures.into_iter())
						.map(|(message, signatures)| withdraw_relay_payload(&app.mainnet_bridge, signatures, message))
						.map(|payload| TransactionRequest {
							from: app.config.mainnet.account.clone(),
							to: Some(mainnet_contract.clone()),
							gas: Some(app.config.txs.withdraw_relay.gas.into()),
							gas_price: Some(app.config.txs.withdraw_relay.gas_price.into()),
							value: None,
							data: Some(payload),
							nonce: None,
							condition: None,
						})
						.map(|request| {
							app.timer.timeout(
								api::send_transaction(&app.connections.mainnet, request),
								app.config.mainnet.request_timeout)
						})
						.collect::<Vec<_>>();
					WithdrawRelayState::RelayWithdraws {
						future: join_all(relays),
						block,
					}
				},
				WithdrawRelayState::RelayWithdraws { ref mut future, block } => {
					let _ = try_ready!(future.poll());
					WithdrawRelayState::Yield(Some(block))
				},
				WithdrawRelayState::Yield(ref mut block) => match block.take() {
					None => WithdrawRelayState::Wait,
					some => return Ok(some.into()),
				}
			};
			self.state = next_state;
		}
	}
}

#[cfg(test)]
mod tests {
	use rustc_hex::FromHex;
	use web3::types::{Log, Bytes};
	use contracts::{mainnet, testnet};
	use super::{signatures_payload, withdraw_relay_payload};

	#[test]
	fn test_signatures_payload() {
		let testnet = testnet::KovanBridge::default();
		let my_address = "0xaff3454fce5edbc8cca8697c15331677e6ebcccc".parse().unwrap();

		let data = "000000000000000000000000aff3454fce5edbc8cca8697c15331677e6ebcccc00000000000000000000000000000000000000000000000000000000000000f0".from_hex().unwrap();

		let log = Log {
			data: data.into(),
			topics: vec!["0xeb043d149eedb81369bec43d4c3a3a53087debc88d2525f13bfaa3eecda28b5c".parse().unwrap()],
			transaction_hash: Some("0x884edad9ce6fa2440d8a54cc123490eb96d2768479d49ff9c7366125a9424364".parse().unwrap()),
			..Default::default()
		};

		let assignment = signatures_payload(&testnet, 2, my_address, log).unwrap().unwrap();
		let expected_message: Bytes = "490a32c600000000000000000000000000000000000000000000000000000000000000f0".from_hex().unwrap().into();
		let expected_signatures: Vec<Bytes> = vec![
			"1812d99600000000000000000000000000000000000000000000000000000000000000f00000000000000000000000000000000000000000000000000000000000000000".from_hex().unwrap().into(),
			"1812d99600000000000000000000000000000000000000000000000000000000000000f00000000000000000000000000000000000000000000000000000000000000001".from_hex().unwrap().into(),
		];
		assert_eq!(expected_message, assignment.message_payload);
		assert_eq!(expected_signatures, assignment.signature_payloads);
	}

	#[test]
	fn test_signatures_payload_not_ours() {
		let testnet = testnet::KovanBridge::default();
		let my_address = "0xaff3454fce5edbc8cca8697c15331677e6ebcccd".parse().unwrap();

		let data = "000000000000000000000000aff3454fce5edbc8cca8697c15331677e6ebcccc00000000000000000000000000000000000000000000000000000000000000f0".from_hex().unwrap();

		let log = Log {
			data: data.into(),
			topics: vec!["0xeb043d149eedb81369bec43d4c3a3a53087debc88d2525f13bfaa3eecda28b5c".parse().unwrap()],
			transaction_hash: Some("0x884edad9ce6fa2440d8a54cc123490eb96d2768479d49ff9c7366125a9424364".parse().unwrap()),
			..Default::default()
		};

		let assignment = signatures_payload(&testnet, 2, my_address, log).unwrap();
		assert_eq!(None, assignment);
	}

	#[test]
	fn test_withdraw_relay_payload() {
		let mainnet = mainnet::EthereumBridge::default();
		let signatures: Vec<Bytes> = vec![
			vec![0x11; 65].into(),
			vec![0x22; 65].into(),
		];
		let message: Bytes = vec![0x33; 84].into();

		let payload = withdraw_relay_payload(&mainnet, signatures, message);
		let expected: Bytes = "9ce318f6000000000000000000000000000000000000000000000000000000000000008000000000000000000000000000000000000000000000000000000000000000e0000000000000000000000000000000000000000000000000000000000000014000000000000000000000000000000000000000000000000000000000000001a00000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000001100000000000000000000000000000000000000000000000000000000000000220000000000000000000000000000000000000000000000000000000000000002111111111111111111111111111111111111111111111111111111111111111122222222222222222222222222222222222222222222222222222222222222220000000000000000000000000000000000000000000000000000000000000002111111111111111111111111111111111111111111111111111111111111111122222222222222222222222222222222222222222222222222222222222222220000000000000000000000000000000000000000000000000000000000000054333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333000000000000000000000000".from_hex().unwrap().into();
		assert_eq!(expected, payload);
	}
}
