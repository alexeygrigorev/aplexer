use super::*;

// -- Inter-agent messaging (docs/inter-agent-messaging-design.md) --

/// Workspace for `send`/`reply`/`inbox`/`ack`/`show`, which take no
/// `--workspace` flag (design doc section 7): `$APLEXER_WORKSPACE`, else
/// cwd. `log`/`gc` accept an explicit override, passed as `explicit`.
pub(crate) fn resolve_message_workspace(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return canonical_workspace(p);
    }
    if let Ok(v) = env::var("APLEXER_WORKSPACE") {
        if !v.is_empty() {
            return canonical_workspace(Path::new(&v));
        }
    }
    canonical_workspace(Path::new("."))
}

/// Resolves the `--to`/`--all`/`--to-engine` triple into a `Recipient`,
/// applying the typo guard of design doc section 2.3: a tag that has never
/// existed in this workspace is rejected with the list of known tags unless
/// `--queue` is passed. Broadcast/engine forms always succeed.
pub(crate) fn build_recipient(
    records: &[SessionRecord],
    workspace: &Path,
    to: Option<&str>,
    all: bool,
    to_engine: Option<&str>,
    queue: bool,
) -> Result<Recipient> {
    let chosen = [to.is_some(), all, to_engine.is_some()]
        .iter()
        .filter(|b| **b)
        .count();
    if chosen == 0 {
        bail!("specify exactly one of --to TAG, --all, or --to-engine ENGINE");
    }
    if chosen > 1 {
        bail!("--to, --all, and --to-engine are mutually exclusive");
    }
    if let Some(tag) = to {
        let existing = session_by_tag(records, workspace, tag);
        if existing.is_none() && !queue {
            let known = known_tags(records, workspace);
            let hint = if known.is_empty() {
                "no session has ever run in this workspace".to_string()
            } else {
                format!("known tags: {}", known.join(", "))
            };
            bail!(
                "no session tagged {tag:?} has ever existed in this workspace ({hint}); pass \
                 --queue to park a message for a session that will be created later"
            );
        }
        return Ok(Recipient::Tag {
            tag: tag.to_string(),
            session_id: existing.map(|r| r.id),
        });
    }
    if all {
        return Ok(Recipient::Broadcast { broadcast: true });
    }
    Ok(Recipient::Engine {
        engine: to_engine.unwrap().to_string(),
    })
}

/// Pane delivery (design doc section 6.2): reuses `a send`'s own PTY-write
/// RPC path (`Operation::Send`, `rpc_send` below) -- the client resolves the
/// target session and connects to its worker socket directly, exactly like
/// `a send <target> <text>` does today. No new server-side RPC operation.
pub(crate) fn deliver_pane(
    records: &[SessionRecord],
    workspace: &Path,
    tag: &str,
    from_tag: Option<&str>,
    body: &str,
    raw: bool,
    no_enter: bool,
) -> Result<()> {
    if body.len() > MAX_BODY_BYTES {
        bail!("message body exceeds the {MAX_BODY_BYTES}-byte cap");
    }
    let record = session_by_tag(records, workspace, tag)
        .ok_or_else(|| anyhow!("no session tagged {tag:?} in this workspace"))?;
    if !record.worker_alive() {
        bail!("session {tag:?} is not running; pane delivery requires a live target");
    }
    rpc_send(record, &pane_input_bytes(body, from_tag, raw, no_enter))
        .with_context(|| format!("inject into session {tag:?}'s PTY"))
}

/// The bytes `--pane` delivery injects: the message, framed with its sender
/// unless `raw`, and -- by default, the tmuxctl behavior -- a trailing
/// return, so a message typed into an agent's prompt actually submits
/// instead of sitting there unconfirmed. `--no-enter` drops the return for
/// the rare target that should compose rather than submit.
pub(crate) fn pane_input_bytes(
    body: &str,
    from_tag: Option<&str>,
    raw: bool,
    no_enter: bool,
) -> Vec<u8> {
    let mut out = if raw {
        body.as_bytes().to_vec()
    } else {
        let sender = from_tag.unwrap_or("external");
        format!("[aplexer message from {sender}] {body}").into_bytes()
    };
    if !no_enter {
        out.push(b'\r');
    }
    out
}

pub(crate) fn parse_data_arg(raw: Option<&str>) -> Result<Option<Value>> {
    raw.map(|s| serde_json::from_str::<Value>(s).context("--data must be valid JSON"))
        .transpose()
}

/// Shared send/reply tail: attempts `--pane` delivery if requested (falling
/// back to inbox on failure iff `--or-inbox`), then always writes the
/// message to the durable mailbox -- pane-delivered messages are recorded
/// too (with `delivery: pane`, pre-acked for the recipient) so the mailbox
/// stays a complete account of inter-agent traffic (design doc section 6.2).
pub(crate) fn finish_send(
    mp: &MessagePaths,
    records: &[SessionRecord],
    workspace: &Path,
    mut envelope: MessageEnvelope,
    pane: &PaneDeliveryArgs,
) -> Result<MessageEnvelope> {
    if pane.pane {
        let Recipient::Tag { tag, .. } = &envelope.to else {
            bail!("--pane requires a single --to TAG target: no pane broadcast");
        };
        let tag = tag.clone();
        match deliver_pane(
            records,
            workspace,
            &tag,
            envelope.from.tag.as_deref(),
            &envelope.body,
            pane.raw,
            pane.no_enter,
        ) {
            Ok(()) => envelope.delivery = Delivery::Pane,
            Err(e) => {
                if pane.or_inbox {
                    eprintln!("a: pane delivery failed ({e:#}); falling back to inbox");
                } else {
                    return Err(e);
                }
            }
        }
    }
    let recorded = match write_message_in(mp, &envelope) {
        Ok(()) => true,
        Err(error) if envelope.delivery == Delivery::Pane => {
            // PTY injection is the pane-delivery commit point. Reporting
            // total failure here invites a retry that injects the same
            // message twice; the missing mailbox copy is only a warning.
            eprintln!(
                "a: pane delivery succeeded, but recording it in the mailbox failed: {error:#}"
            );
            false
        }
        Err(error) => return Err(error),
    };
    if recorded && envelope.delivery == Delivery::Pane {
        if let Recipient::Tag {
            session_id: Some(sid),
            ..
        } = &envelope.to
        {
            // Best-effort: a pane message is already delivered by
            // definition, so a failure to also pre-ack it here is a
            // cosmetic mailbox-record issue, not a delivery failure
            // (design doc section 6.2).
            let _ = ack_messages_in(mp, *sid, &[envelope.id]);
        }
    }
    if recorded {
        let _ = maybe_gc_in(mp, workspace, records);
    }
    Ok(envelope)
}

/// Messages addressed to `consumer` that it has not acknowledged, in id
/// (= time) order.
fn unread_messages(
    mp: &MessagePaths,
    workspace: &Path,
    consumer: &SessionIdentity,
) -> Result<Vec<MessageEnvelope>> {
    let cursor = read_cursor_in(mp, consumer.id)?;
    Ok(list_messages_in(mp, workspace)?
        .into_iter()
        .filter(|m| consumer.receives(m) && !cursor.is_acked(m.id))
        .collect())
}

pub(crate) fn print_message_line(m: &MessageEnvelope) {
    let sender = m.from.tag.clone().unwrap_or_else(|| {
        if m.from.external {
            "external".into()
        } else {
            "?".into()
        }
    });
    let to_desc = match &m.to {
        Recipient::Tag { tag, .. } => format!("to:{tag}"),
        Recipient::Broadcast { .. } => "to:*".to_string(),
        Recipient::Engine { engine } => format!("to:engine:{engine}"),
    };
    let delivery = match m.delivery {
        Delivery::Inbox => "",
        Delivery::Pane => " [pane]",
    };
    let first_line = m.body.lines().next().unwrap_or("");
    println!(
        "{}  [{}] {sender} -> {to_desc}{delivery}  {first_line}",
        m.id, m.kind
    );
}

pub(crate) fn print_message_details(m: &MessageEnvelope) {
    println!("id: {}", m.id);
    println!("workspace: {}", m.workspace.display());
    println!("created_at: {}", m.created_at);
    let sender = m.from.tag.clone().unwrap_or_else(|| {
        if m.from.external {
            "(external)".into()
        } else {
            "(unknown)".into()
        }
    });
    println!(
        "from: {sender}{}",
        m.from
            .engine
            .as_deref()
            .map(|e| format!(" [{e}]"))
            .unwrap_or_default()
    );
    match &m.to {
        Recipient::Tag { tag, .. } => println!("to: {tag}"),
        Recipient::Broadcast { .. } => println!("to: * (broadcast)"),
        Recipient::Engine { engine } => println!("to: engine:{engine}"),
    }
    println!("kind: {}", m.kind);
    if let Some(r) = m.reply_to {
        println!("reply_to: {r}");
    }
    println!(
        "delivery: {}",
        match m.delivery {
            Delivery::Inbox => "inbox",
            Delivery::Pane => "pane",
        }
    );
    println!("---");
    println!("{}", m.body);
    if let Some(d) = &m.data {
        println!("---");
        println!("data: {d}");
    }
}

pub(crate) fn cmd_message(paths: &Paths, args: MessageArgs, json_output: bool) -> Result<()> {
    match args.command {
        MessageCommand::Send(a) => cmd_message_send(paths, a, json_output),
        MessageCommand::Reply(a) => cmd_message_reply(paths, a, json_output),
        MessageCommand::Inbox(a) => cmd_message_inbox(paths, a, json_output),
        MessageCommand::Log(a) => cmd_message_log(paths, a, json_output),
        MessageCommand::Show(a) => cmd_message_show(paths, a, json_output),
        MessageCommand::Ack(a) => cmd_message_ack(paths, a, json_output),
        MessageCommand::Gc(a) => cmd_message_gc(paths, a, json_output),
    }
}

pub(crate) fn cmd_message_send(
    paths: &Paths,
    args: MessageSendArgs,
    json_output: bool,
) -> Result<()> {
    if args.pane_delivery.pane && (args.all || args.to_engine.is_some()) {
        bail!("--pane cannot be combined with --all or --to-engine: no pane broadcast");
    }
    if args.pane_delivery.pane && args.to.is_none() {
        bail!("--pane requires --to TAG");
    }
    check_body_size(&args.text)?;
    let workspace = resolve_message_workspace(None)?;
    let records = list_records(paths)?;
    let data = parse_data_arg(args.data.as_deref())?;
    let from = MessageFrom::from_identity(resolve_identity(
        &records,
        &workspace,
        args.from.as_deref(),
    )?);
    let to = build_recipient(
        &records,
        &workspace,
        args.to.as_deref(),
        args.all,
        args.to_engine.as_deref(),
        args.queue,
    )?;
    let envelope = MessageEnvelope {
        schema_version: MESSAGE_SCHEMA_VERSION,
        id: Uuid::now_v7(),
        workspace: workspace.clone(),
        created_at: now_secs(),
        from,
        to,
        kind: args.kind,
        reply_to: None,
        body: args.text,
        data,
        delivery: Delivery::Inbox,
    };
    let mp = ensure_workspace(paths, &workspace)?;
    let envelope = finish_send(&mp, &records, &workspace, envelope, &args.pane_delivery)?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&envelope)?);
    } else {
        println!("{}", envelope.id);
    }
    Ok(())
}

pub(crate) fn cmd_message_reply(
    paths: &Paths,
    args: MessageReplyArgs,
    json_output: bool,
) -> Result<()> {
    check_body_size(&args.text)?;
    let workspace = resolve_message_workspace(None)?;
    let records = list_records(paths)?;
    let mp = ensure_workspace(paths, &workspace)?;
    let original = read_message_in(&mp, &workspace, args.message_id)
        .with_context(|| format!("no such message {}", args.message_id))?;
    let to_tag = original.from.tag.clone().ok_or_else(|| {
        anyhow!("original message {} was sent anonymously (no sender tag); reply with `a message send --to <tag>` instead", args.message_id)
    })?;
    let data = parse_data_arg(args.data.as_deref())?;
    let from = MessageFrom::from_identity(resolve_identity(
        &records,
        &workspace,
        args.from.as_deref(),
    )?);
    let target = session_by_tag(&records, &workspace, &to_tag);
    let to = Recipient::Tag {
        tag: to_tag,
        session_id: target.map(|r| r.id).or(original.from.session_id),
    };
    let envelope = MessageEnvelope {
        schema_version: MESSAGE_SCHEMA_VERSION,
        id: Uuid::now_v7(),
        workspace: workspace.clone(),
        created_at: now_secs(),
        from,
        to,
        kind: args.kind.unwrap_or_else(|| "reply".to_string()),
        reply_to: Some(original.id),
        body: args.text,
        data,
        delivery: Delivery::Inbox,
    };
    let envelope = finish_send(&mp, &records, &workspace, envelope, &args.pane_delivery)?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&envelope)?);
    } else {
        println!("{}", envelope.id);
    }
    Ok(())
}

pub(crate) fn cmd_message_inbox(
    paths: &Paths,
    args: MessageInboxArgs,
    json_output: bool,
) -> Result<()> {
    let _ = args.new; // `--new` is accepted for CLI-surface compatibility; unread is already the default (design doc section 7).
    let workspace = resolve_message_workspace(None)?;
    let records = list_records(paths)?;
    let consumer = SessionIdentity::required(resolve_identity(
        &records,
        &workspace,
        args.from.as_deref(),
    )?)?;
    let mp = ensure_workspace(paths, &workspace)?;
    let _ = maybe_gc_in(&mp, &workspace, &records);
    let messages = unread_messages(&mp, &workspace, &consumer)?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&messages)?);
    } else if messages.is_empty() {
        println!("no unread messages");
    } else {
        for m in &messages {
            print_message_line(m);
        }
    }
    Ok(())
}

pub(crate) fn cmd_message_log(
    paths: &Paths,
    args: MessageLogArgs,
    json_output: bool,
) -> Result<()> {
    let workspace = resolve_message_workspace(args.workspace.as_deref())?;
    let mp = ensure_workspace(paths, &workspace)?;
    let _ = maybe_gc_in(&mp, &workspace, &list_records(paths)?);
    let messages = list_messages_in(&mp, &workspace)?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&messages)?);
    } else if messages.is_empty() {
        println!("no messages");
    } else {
        for m in &messages {
            print_message_line(m);
        }
    }
    Ok(())
}

pub(crate) fn cmd_message_show(
    paths: &Paths,
    args: MessageShowArgs,
    json_output: bool,
) -> Result<()> {
    let workspace = resolve_message_workspace(None)?;
    let message = read_message(paths, &workspace, args.message_id)?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&message)?);
    } else {
        print_message_details(&message);
    }
    Ok(())
}

pub(crate) fn cmd_message_ack(
    paths: &Paths,
    args: MessageAckArgs,
    json_output: bool,
) -> Result<()> {
    if args.all && !args.message_ids.is_empty() {
        bail!("cannot combine --all with explicit message ids");
    }
    if !args.all && args.message_ids.is_empty() {
        bail!("specify at least one message id, or --all");
    }
    let workspace = resolve_message_workspace(None)?;
    let records = list_records(paths)?;
    let consumer = SessionIdentity::required(resolve_identity(
        &records,
        &workspace,
        args.from.as_deref(),
    )?)?;
    let mp = ensure_workspace(paths, &workspace)?;
    let ids: Vec<Uuid> = if args.all {
        unread_messages(&mp, &workspace, &consumer)?
            .into_iter()
            .map(|m| m.id)
            .collect()
    } else {
        args.message_ids
    };
    let acked = ack_messages_in(&mp, consumer.id, &ids)?;
    let unknown: Vec<Uuid> = ids
        .iter()
        .filter(|id| !acked.contains(id))
        .copied()
        .collect();
    if json_output {
        println!("{}", json!({"acked": acked, "unknown": unknown}));
    } else {
        println!("acked {} message(s)", acked.len());
        if !unknown.is_empty() {
            eprintln!(
                "a: {} id(s) not in this mailbox (pruned, or never here): {}",
                unknown.len(),
                unknown
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }
    Ok(())
}

pub(crate) fn cmd_message_gc(paths: &Paths, args: MessageGcArgs, json_output: bool) -> Result<()> {
    let workspace = resolve_message_workspace(args.workspace.as_deref())?;
    let report = gc_workspace(paths, &workspace)?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "removed {} message(s), {} remaining",
            report.removed, report.remaining
        );
    }
    Ok(())
}
