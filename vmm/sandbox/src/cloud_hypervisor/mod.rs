/*
Copyright 2022 The Kuasar Authors.

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

use std::{os::fd::OwnedFd, process::Stdio, time::Duration};

use anyhow::anyhow;
use async_trait::async_trait;
use containerd_sandbox::error::{Error, Result};
use log::{debug, error, info, warn};
use nix::{errno::Errno::ESRCH, sys::signal, unistd::Pid};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::{
    fs::create_dir_all,
    process::Child,
    sync::watch::{channel, Receiver, Sender},
    task::JoinHandle,
};
use tracing::instrument;
use vmm_common::SHARED_DIR_SUFFIX;

use crate::{
    cloud_hypervisor::{
        client::ChClient,
        config::{CloudHypervisorConfig, CloudHypervisorVMConfig, VirtiofsdConfig},
        devices::{
            block::Disk, vfio::VfioDevice, virtio_net::VirtioNetDevice, CloudHypervisorDevice,
        },
    },
    device::{BusType, DeviceInfo},
    param::ToCmdLineParams,
    utils::{read_std, set_cmd_fd, set_cmd_netns, wait_channel, wait_pid, write_file_atomic},
    vm::{Pids, VcpuThreads, VM},
};

mod client;
pub mod config;
pub mod devices;
pub mod factory;
pub mod hooks;

const VCPU_PREFIX: &str = "vcpu";

#[derive(Default, Serialize, Deserialize)]
pub struct CloudHypervisorVM {
    id: String,
    config: CloudHypervisorConfig,
    #[serde(skip)]
    devices: Vec<Box<dyn CloudHypervisorDevice + Sync + Send>>,
    netns: String,
    base_dir: String,
    agent_socket: String,
    virtiofsd_config: VirtiofsdConfig,
    #[serde(skip)]
    wait_chan: Option<Receiver<(u32, i128)>>,
    #[serde(skip)]
    client: Option<ChClient>,
    #[serde(skip)]
    fds: Vec<OwnedFd>,
    pids: Pids,
}

impl CloudHypervisorVM {
    pub fn new(id: &str, netns: &str, base_dir: &str, vm_config: &CloudHypervisorVMConfig) -> Self {
        let mut config = CloudHypervisorConfig::from(vm_config);
        config.api_socket = format!("{}/api.sock", base_dir);
        if !vm_config.common.initrd_path.is_empty() {
            config.initramfs = Some(vm_config.common.initrd_path.clone());
        }

        let mut virtiofsd_config = vm_config.virtiofsd.clone();
        virtiofsd_config.socket_path = format!("{}/virtiofs.sock", base_dir);
        virtiofsd_config.shared_dir = format!("{}/{}", base_dir, SHARED_DIR_SUFFIX);
        Self {
            id: id.to_string(),
            config,
            devices: vec![],
            netns: netns.to_string(),
            base_dir: base_dir.to_string(),
            agent_socket: "".to_string(),
            virtiofsd_config,
            wait_chan: None,
            client: None,
            fds: vec![],
            pids: Pids::default(),
        }
    }

    pub fn add_device(&mut self, device: impl CloudHypervisorDevice + 'static) {
        self.devices.push(Box::new(device));
    }

    fn pid(&self) -> Result<u32> {
        match self.pids.vmm_pid {
            None => Err(anyhow!("empty pid from vmm_pid").into()),
            Some(pid) => Ok(pid),
        }
    }

    async fn create_client(&self) -> Result<ChClient> {
        ChClient::new(self.config.api_socket.to_string()).await
    }

    fn get_client(&mut self) -> Result<&mut ChClient> {
        self.client.as_mut().ok_or(Error::NotFound(
            "cloud hypervisor client not inited".to_string(),
        ))
    }

    async fn start_virtiofsd(&self) -> Result<u32> {
        create_dir_all(&self.virtiofsd_config.shared_dir).await?;
        let params = self.virtiofsd_config.to_cmdline_params("--");
        let mut cmd = tokio::process::Command::new(&self.virtiofsd_config.path);
        cmd.args(params.as_slice());
        debug!("start virtiofsd with cmdline: {:?}", cmd);
        set_cmd_netns(&mut cmd, self.netns.to_string())?;
        cmd.stderr(Stdio::piped());
        cmd.stdout(Stdio::piped());
        let child = cmd
            .spawn()
            .map_err(|e| anyhow!("failed to spawn virtiofsd command: {}", e))?;
        let pid = child
            .id()
            .ok_or(anyhow!("the virtiofsd has been polled to completion"))?;
        info!("virtiofsd for {} is running with pid {}", self.id, pid);
        spawn_wait(child, format!("virtiofsd {}", self.id), None, None);
        Ok(pid)
    }

    fn append_fd(&mut self, fd: OwnedFd) -> usize {
        self.fds.push(fd);
        self.fds.len() - 1 + 3
    }

    async fn wait_stop(&mut self, t: Duration) -> Result<()> {
        if let Some(rx) = self.wait_channel().await {
            let (_, ts) = *rx.borrow();
            if ts == 0 {
                wait_channel(t, rx).await?;
            }
        }
        Ok(())
    }
}

#[async_trait]
impl VM for CloudHypervisorVM {
    #[instrument(skip_all)]
    async fn start(&mut self) -> Result<u32> {
        create_dir_all(&self.base_dir).await?;
        let virtiofsd_pid = self.start_virtiofsd().await?;
        // TODO: add child virtiofsd process
        self.pids.affiliated_pids.push(virtiofsd_pid);
        let mut params = self.config.to_cmdline_params("--");
        for d in self.devices.iter() {
            params.extend(d.to_cmdline_params("--"));
        }

        // the log level is single hyphen parameter, has to handle separately
        if self.config.debug {
            params.push("-vv".to_string());
        }

        // Drop cmd immediately to let the fds in pre_exec be closed.
        let child = {
            let mut cmd = tokio::process::Command::new(&self.config.path);
            cmd.args(params.as_slice());

            set_cmd_fd(&mut cmd, self.fds.drain(..).collect())?;
            set_cmd_netns(&mut cmd, self.netns.to_string())?;
            cmd.stdout(Stdio::piped());
            cmd.stderr(Stdio::piped());
            info!("start cloud hypervisor with cmdline: {:?}", cmd);
            cmd.spawn()
                .map_err(|e| anyhow!("failed to spawn cloud hypervisor command: {}", e))?
        };
        let pid = child.id();
        info!(
            "cloud hypervisor for {} is running with pid {}",
            self.id,
            pid.unwrap_or_default()
        );
        self.pids.vmm_pid = pid;
        let pid_file = format!("{}/pid", self.base_dir);
        let (tx, rx) = channel((0u32, 0i128));
        self.wait_chan = Some(rx);
        spawn_wait(
            child,
            format!("cloud-hypervisor {}", self.id),
            Some(pid_file),
            Some(tx),
        );

        match self.create_client().await {
            Ok(client) => self.client = Some(client),
            Err(e) => {
                if let Err(re) = self.stop(true).await {
                    warn!("roll back in create clh api client: {}", re);
                    return Err(e);
                }
                return Err(e);
            }
        };
        Ok(pid.unwrap_or_default())
    }

    #[instrument(skip_all)]
    async fn stop(&mut self, force: bool) -> Result<()> {
        let signal = if force {
            signal::SIGKILL
        } else {
            signal::SIGTERM
        };

        let pids = self.pids();
        if let Some(vmm_pid) = pids.vmm_pid {
            if vmm_pid > 0 {
                // TODO: Consider pid reused
                match signal::kill(Pid::from_raw(vmm_pid as i32), signal) {
                    Err(e) => {
                        if e != ESRCH {
                            return Err(anyhow!("kill vmm process {}: {}", vmm_pid, e).into());
                        }
                    }
                    Ok(_) => self.wait_stop(Duration::from_secs(10)).await?,
                }
            }
        }
        for affiliated_pid in pids.affiliated_pids {
            if affiliated_pid > 0 {
                // affiliated process may exits automatically, so it's ok not handle error
                signal::kill(Pid::from_raw(affiliated_pid as i32), signal).unwrap_or_default();
            }
        }

        Ok(())
    }

    #[instrument(skip_all)]
    async fn attach(&mut self, device_info: DeviceInfo) -> Result<()> {
        match device_info {
            DeviceInfo::Block(blk_info) => {
                let device = Disk::new(&blk_info.id, &blk_info.path, blk_info.read_only, true);
                self.add_device(device);
            }
            DeviceInfo::Tap(tap_info) => {
                let mut fd_ints = vec![];
                for fd in tap_info.fds {
                    let index = self.append_fd(fd);
                    fd_ints.push(index as i32);
                }
                let device = VirtioNetDevice::new(
                    &tap_info.id,
                    Some(tap_info.name),
                    &tap_info.mac_address,
                    fd_ints,
                );
                self.add_device(device);
            }
            DeviceInfo::Physical(vfio_info) => {
                let device = VfioDevice::new(&vfio_info.id, &vfio_info.bdf);
                self.add_device(device);
            }
            DeviceInfo::VhostUser(_vhost_user_info) => {
                todo!()
            }
            DeviceInfo::Char(_char_info) => {
                unimplemented!()
            }
        };
        Ok(())
    }

    #[instrument(skip_all)]
    async fn hot_attach(&mut self, device_info: DeviceInfo) -> Result<(BusType, String)> {
        let client = self.get_client()?;
        let addr = client.hot_attach(device_info)?;
        Ok((BusType::PCI, addr))
    }

    #[instrument(skip_all)]
    async fn hot_detach(&mut self, id: &str) -> Result<()> {
        let client = self.get_client()?;
        client.hot_detach(id)?;
        Ok(())
    }

    #[instrument(skip_all)]
    async fn ping(&self) -> Result<()> {
        // TODO
        Ok(())
    }

    #[instrument(skip_all)]
    fn socket_address(&self) -> String {
        self.agent_socket.to_string()
    }

    #[instrument(skip_all)]
    async fn wait_channel(&self) -> Option<Receiver<(u32, i128)>> {
        self.wait_chan.clone()
    }

    #[instrument(skip_all)]
    async fn vcpus(&self) -> Result<VcpuThreads> {
        // Refer to https://github.com/firecracker-microvm/firecracker/issues/718
        Ok(VcpuThreads {
            vcpus: procfs::process::Process::new(self.pid()? as i32)
                .map_err(|e| anyhow!("failed to get process {}", e))?
                .tasks()
                .map_err(|e| anyhow!("failed to get tasks {}", e))?
                .flatten()
                .filter_map(|t| {
                    t.stat()
                        .map_err(|e| anyhow!("failed to get stat {}", e))
                        .ok()?
                        .comm
                        .strip_prefix(VCPU_PREFIX)
                        .and_then(|comm| comm.parse().ok())
                        .map(|index| (index, t.tid as i64))
                })
                .collect(),
        })
    }

    #[instrument(skip_all)]
    fn pids(&self) -> Pids {
        self.pids.clone()
    }
}

#[async_trait]
impl crate::vm::Recoverable for CloudHypervisorVM {
    #[instrument(skip_all)]
    async fn recover(&mut self) -> Result<()> {
        self.client = Some(self.create_client().await?);
        let pid = self.pid()?;
        let (tx, rx) = channel((0u32, 0i128));
        tokio::spawn(async move {
            let wait_result = wait_pid(pid as i32).await;
            tx.send(wait_result).unwrap_or_default();
        });
        self.wait_chan = Some(rx);
        Ok(())
    }
}

macro_rules! read_stdio {
    ($stdio:expr, $cmd_name:ident) => {
        if let Some(std) = $stdio {
            let cmd_name_clone = $cmd_name.clone();
            tokio::spawn(async move {
                read_std(std, &cmd_name_clone).await.unwrap_or_default();
            });
        }
    };
}

fn spawn_wait(
    child: Child,
    cmd_name: String,
    pid_file_path: Option<String>,
    exit_chan: Option<Sender<(u32, i128)>>,
) -> JoinHandle<()> {
    let mut child = child;
    tokio::spawn(async move {
        if let Some(pid_file) = pid_file_path {
            if let Some(pid) = child.id() {
                write_file_atomic(&pid_file, &pid.to_string())
                    .await
                    .unwrap_or_default();
            }
        }

        read_stdio!(child.stdout.take(), cmd_name);
        read_stdio!(child.stderr.take(), cmd_name);

        match child.wait().await {
            Ok(status) => {
                if !status.success() {
                    error!("{} exit {}", cmd_name, status);
                }
                let now = OffsetDateTime::now_utc();
                if let Some(tx) = exit_chan {
                    tx.send((
                        status.code().unwrap_or_default() as u32,
                        now.unix_timestamp_nanos(),
                    ))
                    .unwrap_or_default();
                }
            }
            Err(e) => {
                error!("{} wait error {}", cmd_name, e);
                let now = OffsetDateTime::now_utc();
                if let Some(tx) = exit_chan {
                    tx.send((0, now.unix_timestamp_nanos())).unwrap_or_default();
                }
            }
        }
    })
}
