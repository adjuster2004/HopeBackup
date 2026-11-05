//! AWS EC2: новый инстанс из AMI (по умолчанию — последний Ubuntu 24.04 от Canonical), ключ и
//! пользователь — через cloud-init (UserData). API — Query (Version 2016-11-15), подпись SigV4.

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use base64::Engine;
use serde_json::{json, Value};
use std::time::Duration;

use super::{cloud_config, p_bool, p_list, p_raw, p_str, p_u64, Provider, VmInfo, VmRequest};
use crate::cloud::sigv4::{self, Creds};
use crate::cloud::xml_tag;
use crate::plugins::LogFn;

const CANONICAL: &str = "099720109477";

pub struct Aws {
    c: reqwest::Client,
    endpoint: String,
    host: String,
    region: String,
    p: Value,
}

/// Записи списка, начинающиеся с тега `first` (элементы `item` бывают вложенными — режем по первому полю).
pub fn records<'a>(xml: &'a str, first: &str) -> Vec<&'a str> {
    let open = format!("<{first}>");
    let idx: Vec<usize> = xml.match_indices(&open).map(|(i, _)| i).collect();
    idx.iter().enumerate().map(|(n, &i)| &xml[i..idx.get(n + 1).copied().unwrap_or(xml.len())]).collect()
}

impl Aws {
    pub fn new(p: Value) -> Result<Aws> {
        let region = { let r = p_str(&p, "region"); if r.is_empty() { "eu-central-1".to_string() } else { r } };
        if p_str(&p, "access_key").is_empty() || p_raw(&p, "secret_key").is_empty() { bail!("не заданы ключи доступа AWS"); }
        if p_str(&p, "subnet_id").is_empty() { bail!("не задана подсеть (subnet_id)"); }
        let endpoint = { let e = p_str(&p, "endpoint"); if e.is_empty() { format!("https://ec2.{region}.amazonaws.com") } else { e.trim_end_matches('/').to_string() } };
        let host = endpoint.split_once("://").map(|x| x.1).unwrap_or(&endpoint).split('/').next().unwrap_or("").to_string();
        Ok(Aws { c: crate::tlsutil::client(None)?, endpoint, host, region, p })
    }

    async fn call(&self, action: &str, params: Vec<(String, String)>) -> Result<String> {
        let mut form = vec![("Action".to_string(), action.to_string()), ("Version".to_string(), "2016-11-15".to_string())];
        form.extend(params);
        let body = form.iter().map(|(k, v)| format!("{}={}", sigv4::uri_encode(k, false), sigv4::uri_encode(v, false))).collect::<Vec<_>>().join("&");
        let ak = p_str(&self.p, "access_key");
        let sk = p_raw(&self.p, "secret_key");
        let creds = Creds { access_key: &ak, secret_key: sk.trim(), region: &self.region, service: "ec2" };
        let ct = ("content-type".to_string(), "application/x-www-form-urlencoded; charset=utf-8".to_string());
        let signed = sigv4::sign(&creds, "POST", &self.host, "/", &[], &[ct], &sigv4::sha256_hex(body.as_bytes()), chrono::Utc::now());
        let mut r = self.c.post(format!("{}/", self.endpoint)).timeout(Duration::from_secs(60)).body(body);
        for (k, v) in signed { r = r.header(k, v); }
        let resp = r.send().await.map_err(|e| anyhow!("AWS EC2 {action}: {e}"))?;
        let st = resp.status();
        let x = resp.text().await.unwrap_or_default();
        if !st.is_success() {
            bail!("AWS EC2 {action}: {} — {}", xml_tag(&x, "Code").unwrap_or_else(|| st.to_string()), xml_tag(&x, "Message").unwrap_or_default());
        }
        Ok(x)
    }

    /// AMI и имя корневого устройства.
    async fn ami(&self) -> Result<(String, String)> {
        let id = p_str(&self.p, "ami");
        let x = if !id.is_empty() {
            self.call("DescribeImages", vec![("ImageId.1".into(), id.clone())]).await?
        } else {
            self.call("DescribeImages", vec![
                ("Owner.1".into(), CANONICAL.into()),
                ("Filter.1.Name".into(), "name".into()), ("Filter.1.Value.1".into(), "ubuntu/images/hvm-ssd*/ubuntu-noble-24.04-amd64-server-*".into()),
                ("Filter.2.Name".into(), "state".into()), ("Filter.2.Value.1".into(), "available".into()),
            ]).await?
        };
        let mut best: Option<(String, String, String)> = None;
        for r in records(&x, "imageId") {
            let Some(img) = xml_tag(r, "imageId") else { continue };
            let created = xml_tag(r, "creationDate").unwrap_or_default();
            let root = xml_tag(r, "rootDeviceName").unwrap_or_else(|| "/dev/sda1".into());
            if best.as_ref().map(|b| created > b.1).unwrap_or(true) { best = Some((img, created, root)); }
        }
        let (img, _, root) = best.ok_or_else(|| anyhow!("AWS: образ {} не найден", if id.is_empty() { "Ubuntu 24.04 (Canonical)" } else { &id }))?;
        Ok((img, root))
    }

    async fn describe(&self, id: &str) -> Result<(String, Option<String>, Option<String>)> {
        let x = self.call("DescribeInstances", vec![("InstanceId.1".into(), id.into())]).await?;
        let state = xml_tag(&x, "instanceState").and_then(|b| xml_tag(&format!("<s>{b}</s>"), "name")).unwrap_or_default();
        Ok((state, xml_tag(&x, "privateIpAddress"), xml_tag(&x, "ipAddress")))
    }
}

#[async_trait]
impl Provider for Aws {
    fn title(&self) -> &'static str { "AWS EC2" }

    async fn test(&self) -> Result<String> {
        let sub = p_str(&self.p, "subnet_id");
        let x = self.call("DescribeSubnets", vec![("SubnetId.1".into(), sub.clone())]).await?;
        let vpc = xml_tag(&x, "vpcId").unwrap_or_default();
        let az = xml_tag(&x, "availabilityZone").unwrap_or_default();
        let (ami, _) = self.ami().await?;
        Ok(format!("AWS EC2 {}: подсеть {sub} ({vpc}, {az}), AMI {ami}", self.region))
    }

    async fn create(&self, req: &VmRequest, log: &LogFn) -> Result<VmInfo> {
        let p = &req.p;
        let (ami, root) = self.ami().await?;
        let public = p_bool(p, "public_ip", false);
        let itype = { let t = p_str(p, "instance_type"); if t.is_empty() { "t3.large".to_string() } else { t } };
        let ud = base64::engine::general_purpose::STANDARD.encode(cloud_config(&req.user, &req.ssh_pubkey, &req.name));
        let mut q: Vec<(String, String)> = vec![
            ("ImageId".into(), ami.clone()),
            ("InstanceType".into(), itype.clone()),
            ("MinCount".into(), "1".into()),
            ("MaxCount".into(), "1".into()),
            ("UserData".into(), ud),
            ("NetworkInterface.1.DeviceIndex".into(), "0".into()),
            ("NetworkInterface.1.SubnetId".into(), p_str(p, "subnet_id")),
            ("NetworkInterface.1.AssociatePublicIpAddress".into(), public.to_string()),
            ("NetworkInterface.1.DeleteOnTermination".into(), "true".into()),
            ("MetadataOptions.HttpTokens".into(), "required".into()),
            ("TagSpecification.1.ResourceType".into(), "instance".into()),
            ("TagSpecification.1.Tag.1.Key".into(), "Name".into()),
            ("TagSpecification.1.Tag.1.Value".into(), req.name.clone()),
            ("TagSpecification.1.Tag.2.Key".into(), "hopebackup".into()),
            ("TagSpecification.1.Tag.2.Value".into(), "restore".into()),
        ];
        for (i, sg) in p_list(p, "security_group_ids").iter().enumerate() { q.push((format!("NetworkInterface.1.SecurityGroupId.{}", i + 1), sg.clone())); }
        let gb = p_u64(p, "disk_gb", 0);
        if gb > 0 {
            q.push(("BlockDeviceMapping.1.DeviceName".into(), root));
            q.push(("BlockDeviceMapping.1.Ebs.VolumeSize".into(), gb.to_string()));
            q.push(("BlockDeviceMapping.1.Ebs.VolumeType".into(), { let v = p_str(p, "volume_type"); if v.is_empty() { "gp3".to_string() } else { v } }));
            q.push(("BlockDeviceMapping.1.Ebs.DeleteOnTermination".into(), "true".into()));
        }
        log(&format!("запуск инстанса {itype} из {ami}"));
        let x = self.call("RunInstances", q).await?;
        let id = xml_tag(&x, "instanceId").ok_or_else(|| anyhow!("AWS: RunInstances не вернул instanceId"))?;
        let started = std::time::Instant::now();
        let r: Result<String> = async {
            loop {
                let (state, private, public_ip) = self.describe(&id).await?;
                match state.as_str() {
                    "running" => {
                        let ip = if public { public_ip } else { private };
                        if let Some(ip) = ip { return Ok(ip); }
                    }
                    "terminated" | "shutting-down" | "stopped" => bail!("инстанс {id} перешёл в состояние {state}"),
                    _ => {}
                }
                if started.elapsed() > Duration::from_secs(600) { bail!("инстанс {id} не запустился за 600 с (состояние {state})"); }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
        .await;
        match r {
            Ok(ip) => Ok(VmInfo { id: id.clone(), ip, details: json!({ "instance_id": id, "ami": ami, "region": self.region, "instance_type": itype }) }),
            Err(e) => {
                if p_bool(p, "destroy_on_failure", true) { let _ = self.destroy(&id, log).await; }
                Err(e)
            }
        }
    }

    async fn destroy(&self, id: &str, log: &LogFn) -> Result<()> {
        self.call("TerminateInstances", vec![("InstanceId.1".into(), id.into())]).await?;
        log(&format!("инстанс {id} удаляется (TerminateInstances)"));
        Ok(())
    }
}
