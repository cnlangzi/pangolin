package tunnel

import (
	"testing"
)

func TestManager(t *testing.T) {
	mgr := NewManager()

	// 测试初始状态
	if mgr.Count() != 0 {
		t.Errorf("初始隧道数应为0，实际 %d", mgr.Count())
	}

	// 测试获取不存在的隧道
	_, ok := mgr.Get("notexist.test.com")
	if ok {
		t.Error("不存在的隧道应返回false")
	}

	t.Log("✅ Manager基础测试通过")
}

func TestTunnelList(t *testing.T) {
	mgr := NewManager()

	// 空列表
	list := mgr.List()
	if len(list) != 0 {
		t.Errorf("空列表长度应为0，实际 %d", len(list))
	}

	t.Log("✅ 隧道列表测试通过")
}

func TestMessageTypes(t *testing.T) {
	// 验证消息类型常量
	if MsgTypeRegister != "register" {
		t.Errorf("MsgTypeRegister 应为 register，实际 %s", MsgTypeRegister)
	}
	if MsgTypeRequest != "request" {
		t.Errorf("MsgTypeRequest 应为 request，实际 %s", MsgTypeRequest)
	}
	if MsgTypeResponse != "response" {
		t.Errorf("MsgTypeResponse 应为 response，实际 %s", MsgTypeResponse)
	}
	if MsgTypePing != "ping" {
		t.Errorf("MsgTypePing 应为 ping，实际 %s", MsgTypePing)
	}
	if MsgTypePong != "pong" {
		t.Errorf("MsgTypePong 应为 pong，实际 %s", MsgTypePong)
	}

	t.Log("✅ 消息类型测试通过")
}

func TestMessageStruct(t *testing.T) {
	// 测试消息结构
	msg := Message{
		Type:    MsgTypeRegister,
		Domain:  "test.example.com",
		Token:   "secret-token",
	}

	if msg.Type != "register" {
		t.Errorf("消息类型错误")
	}
	if msg.Domain != "test.example.com" {
		t.Errorf("消息域名错误")
	}
	if msg.Token != "secret-token" {
		t.Errorf("消息token错误")
	}

	t.Log("✅ 消息结构测试通过")
}
