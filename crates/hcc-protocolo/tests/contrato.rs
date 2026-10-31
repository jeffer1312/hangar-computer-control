use hcc_protocolo::*;
use serde_json::Value;
fn golden(name: &str) -> Value { serde_json::from_str(&std::fs::read_to_string(format!("{}/tests/golden/{name}.json", env!("CARGO_MANIFEST_DIR"))).unwrap()).unwrap() }

#[test] fn observation_roundtrips_python_shape() {
    let v = golden("observacao");
    let o: Observation = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(serde_json::to_value(&o).unwrap(), v);
    assert!(o.elements.iter().any(|e| e.password && e.value.is_none()));
}
#[test] fn commands_roundtrip() {
    for c in golden("comandos").as_array().unwrap() {
        let cmd: Command = serde_json::from_value(c.clone()).unwrap();
        assert_eq!(&serde_json::to_value(&cmd).unwrap(), c);
    }
}
#[test] fn invalid_actions_are_refused_with_python_messages() {
    for case in golden("acoes_invalidas").as_array().unwrap() {
        let want = case[1].as_str().unwrap();
        let got = serde_json::from_value::<Action>(case[0].clone()).map_err(|e| e.to_string()).and_then(|a| a.validate().map(|_| a));
        assert!(got.as_ref().err().is_some_and(|e| e.contains(want)), "{case}: {got:?}");
    }
}
#[test] fn hello_carries_protocol_3_and_password_flag_is_omitted_when_false() {
    let h = AgentPost { hello: true, boot: "b".into(), session_id: 1, pid: 2, protocol: Some(PROTOCOL), ..Default::default() };
    assert_eq!(serde_json::to_value(&h).unwrap(), serde_json::json!({"hello":true,"boot":"b","session_id":1,"pid":2,"protocol":3}));
}
#[test] fn act_result_without_delivery_reads_as_unknown() {
    let r: ActResult = serde_json::from_value(serde_json::json!({"ok": true})).unwrap();
    assert_eq!(serde_json::to_value(r).unwrap(), serde_json::json!({"ok": true, "delivery": "unknown"}));
    let r: ActResult = serde_json::from_value(serde_json::json!({"ok": true, "delivery": "delivered_verified"})).unwrap();
    assert_eq!(r.delivery, Delivery::DeliveredVerified);
}
#[test] fn act_error_carries_not_delivered_and_default_scope_is_omitted() {
    let e: AgentPost = serde_json::from_value(serde_json::json!({"boot":"b","session_id":1,"pid":2,"id":"a1","error":"ValueError: x","delivery":"not_delivered"})).unwrap();
    assert_eq!(e.delivery, Some(Delivery::NotDelivered));
    let c: Command = serde_json::from_value(serde_json::json!({"operation":"observe","id":"a1","ttl_ms":1})).unwrap();
    assert_eq!(c, Command::Observe { id: "a1".into(), ttl_ms: 1, scope: Scope::default() });
    assert_eq!(serde_json::to_value(&c).unwrap(), serde_json::json!({"operation":"observe","id":"a1","ttl_ms":1}));
}
#[test] fn connection_debug_hides_token() {
    let c = Connection { url: "http://127.0.0.1:1/next".into(), token: "segredo".into() };
    assert!(!format!("{c:?}").contains("segredo"));
}
