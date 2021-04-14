use lightning::chain::channelmonitor;
use lightning::chain::transaction::OutPoint;
use lightning::util::enforcing_trait_impls::EnforcingSigner;

pub struct TestPersister {}
impl channelmonitor::Persist<EnforcingSigner> for TestPersister {
	fn persist_new_channel(&self, _funding_txo: OutPoint, _data: &channelmonitor::ChannelMonitor<EnforcingSigner>) -> Result<(), channelmonitor::ChannelMonitorUpdateErr> {
		Ok(())
	}

	fn update_persisted_channel(&self, _funding_txo: OutPoint, _update: &channelmonitor::ChannelMonitorUpdate, _data: &channelmonitor::ChannelMonitor<EnforcingSigner>) -> Result<(), channelmonitor::ChannelMonitorUpdateErr> {
		Ok(())
	}
}
