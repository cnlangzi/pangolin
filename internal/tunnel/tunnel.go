package tunnel

import (
	"encoding/json"
	"log"
	"sync"
	"time"

	"github.com/gorilla/websocket"
)

// MessageType 消息类型
const (
	MsgTypeRegister  = "register"
	MsgTypeRequest   = "request"
	MsgTypeResponse  = "response"
	MsgTypePing      = "ping"
	MsgTypePong      = "pong"
)

// Message WebSocket消息
type Message struct {
	Type    string          `json:"type"`
	ID      string          `json:"id,omitempty"`
	Domain  string          `json:"domain,omitempty"`
	Token   string          `json:"token,omitempty"`
	Method  string          `json:"method,omitempty"`
	URL     string          `json:"url,omitempty"`
	Headers map[string]string `json:"headers,omitempty"`
	Status  int             `json:"status,omitempty"`
	Body    string          `json:"body,omitempty"`
}

// Tunnel 内网隧道 (WebSocket)
type Tunnel struct {
	Domain     string
	LocalIP    string
	LocalPort  int
	Conn       *websocket.Conn
	Connected  time.Time
	Online     bool
	RequestCount int
}

// HTTPTunnel HTTP轮询模式隧道
type HTTPTunnel struct {
	Domain      string
	LocalIP     string
	LocalPort   int
	Connected   time.Time
	Online      bool
	RequestCh   chan *Message  // 请求通道
	ResponseCh chan *Message  // 响应通道
	mu          sync.RWMutex
}

// Manager 隧道管理器
type Manager struct {
	tunnels    map[string]*Tunnel     // domain -> ws tunnel
	httpTunnel *HTTPTunnel            // 只有一个HTTP客户端
	mu         sync.RWMutex
}

func NewManager() *Manager {
	return &Manager{
		tunnels: make(map[string]*Tunnel),
	}
}

// Register 注册隧道
func (m *Manager) Register(domain, token, localIP string, localPort int, conn *websocket.Conn) error {
	m.mu.Lock()
	defer m.mu.Unlock()
	
	tunnel := &Tunnel{
		Domain:    domain,
		LocalIP:   localIP,
		LocalPort: localPort,
		Conn:      conn,
		Connected: time.Now(),
		Online:    true,
	}
	
	m.tunnels[domain] = tunnel
	log.Printf("✅ 隧道注册: %s -> %s:%d (共 %d 个)", domain, localIP, localPort, len(m.tunnels))
	
	return nil
}

// Unregister 移除隧道
func (m *Manager) Unregister(domain string) {
	m.mu.Lock()
	defer m.mu.Unlock()
	
	if t, ok := m.tunnels[domain]; ok {
		t.Online = false
		t.Conn.Close()
		delete(m.tunnels, domain)
		log.Printf("❌ 隧道断开: %s (剩余 %d 个)", domain, len(m.tunnels))
	}
}

// Get 获取隧道
func (m *Manager) Get(domain string) (*Tunnel, bool) {
	m.mu.RLock()
	defer m.mu.RUnlock()
	
	t, ok := m.tunnels[domain]
	return t, ok
}

// List 获取所有隧道
func (m *Manager) List() []*Tunnel {
	m.mu.RLock()
	defer m.mu.RUnlock()
	
	tunnels := make([]*Tunnel, 0, len(m.tunnels))
	for _, t := range m.tunnels {
		tunnels = append(tunnels, t)
	}
	return tunnels
}

// Count 在线数量
func (m *Manager) Count() int {
	m.mu.RLock()
	defer m.mu.RUnlock()
	return len(m.tunnels)
}

// SendRequest 发送HTTP请求到隧道
func (t *Tunnel) SendRequest(msg *Message) error {
	t.RequestCount++
	return t.Conn.WriteJSON(msg)
}

// ReadResponse 从隧道读取响应
func (t *Tunnel) ReadResponse(msg *Message) error {
	return t.Conn.ReadJSON(msg)
}

// SendPing 发送心跳
func (t *Tunnel) SendPing() error {
	return t.Conn.WriteJSON(Message{Type: MsgTypePing})
}

// ReadMessage 读取消息
func (t *Tunnel) ReadMessage(msg *Message) error {
	return t.Conn.ReadJSON(msg)
}

// MarshalJSON 序列化
func (t *Tunnel) MarshalJSON() string {
	data, _ := json.Marshal(map[string]interface{}{
		"domain":         t.Domain,
		"local_ip":      t.LocalIP,
		"local_port":    t.LocalPort,
		"online":        t.Online,
		"connected_at": t.Connected.Format("2006-01-02 15:04:05"),
		"request_count": t.RequestCount,
	})
	return string(data)
}

// RegisterHTTP 注册HTTP隧道
func (m *Manager) RegisterHTTP(domain, localIP string, localPort int) {
	m.mu.Lock()
	defer m.mu.Unlock()

	m.httpTunnel = &HTTPTunnel{
		Domain:     domain,
		LocalIP:    localIP,
		LocalPort:  localPort,
		Connected:  time.Now(),
		Online:     true,
		RequestCh:  make(chan *Message, 10),
		ResponseCh: make(chan *Message, 10),
	}
	log.Printf("✅ [HTTP] 隧道注册: %s -> %s:%d", domain, localIP, localPort)
}

// GetHTTPTunnel 获取HTTP隧道
func (m *Manager) GetHTTPTunnel(domain string) *HTTPTunnel {
	m.mu.RLock()
	defer m.mu.RUnlock()
	return m.httpTunnel
}

// ListHTTP 获取所有HTTP隧道
func (m *Manager) ListHTTP() []*HTTPTunnel {
	m.mu.RLock()
	defer m.mu.RUnlock()

	if m.httpTunnel == nil {
		return nil
	}
	return []*HTTPTunnel{m.httpTunnel}
}

// Poll HTTP客户端轮询获取请求
func (t *HTTPTunnel) Poll() (*Message, error) {
	// 非阻塞读取
	select {
	case req := <-t.RequestCh:
		return req, nil
	default:
		return nil, nil
	}
}

// SendResponse 发送HTTP响应
func (t *HTTPTunnel) SendResponse(msg *Message) {
	select {
	case t.ResponseCh <- msg:
	default:
	}
}
