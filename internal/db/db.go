package db

import (
	"database/sql"
	"log"
	"os"
	"time"

	_ "github.com/mattn/go-sqlite3"
)

var db *sql.DB

// Domain 注册域名记录
type Domain struct {
	ID          int
	Domain      string
	LocalIP     string
	LocalPort   int
	Online      bool
	RegisteredAt time.Time
	LastSeenAt  time.Time
}

func Init(dbPath string) error {
	// 创建数据库目录
	os.MkdirAll(dbPath[:len(dbPath)-len("/pangolin.db")], 0755)

	var err error
	db, err = sql.Open("sqlite3", dbPath)
	if err != nil {
		return err
	}

	// 创建表
	_, err = db.Exec(`
		CREATE TABLE IF NOT EXISTS domains (
			id INTEGER PRIMARY KEY AUTOINCREMENT,
			domain TEXT UNIQUE NOT NULL,
			local_ip TEXT NOT NULL,
			local_port INTEGER NOT NULL,
			online INTEGER DEFAULT 0,
			registered_at DATETIME DEFAULT CURRENT_TIMESTAMP,
			last_seen_at DATETIME DEFAULT CURRENT_TIMESTAMP
		);
		CREATE INDEX IF NOT EXISTS idx_domain ON domains(domain);
	`)
	if err != nil {
		return err
	}

	log.Printf("📂 数据库初始化: %s", dbPath)
	return nil
}

// Register 注册/更新域名
func Register(domain, localIP string, localPort int) error {
	_, err := db.Exec(`
		INSERT INTO domains (domain, local_ip, local_port, online, registered_at, last_seen_at)
		VALUES (?, ?, ?, 1, datetime('now'), datetime('now'))
		ON CONFLICT(domain) DO UPDATE SET
			local_ip = excluded.local_ip,
			local_port = excluded.local_port,
			online = 1,
			last_seen_at = datetime('now')
	`, domain, localIP, localPort)
	return err
}

// SetOffline 设置域名离线
func SetOffline(domain string) error {
	_, err := db.Exec("UPDATE domains SET online = 0 WHERE domain = ?", domain)
	return err
}

// IsRegistered 检查域名是否注册过
func IsRegistered(domain string) (bool, *Domain, error) {
	var d Domain
	err := db.QueryRow(`
		SELECT id, domain, local_ip, local_port, online, registered_at, last_seen_at
		FROM domains WHERE domain = ?
	`, domain).Scan(&d.ID, &d.Domain, &d.LocalIP, &d.LocalPort, &d.Online, &d.RegisteredAt, &d.LastSeenAt)
	
	if err == sql.ErrNoRows {
		return false, nil, nil
	}
	if err != nil {
		return false, nil, err
	}
	return true, &d, nil
}

// GetAll 获取所有域名
func GetAll() ([]*Domain, error) {
	rows, err := db.Query(`
		SELECT id, domain, local_ip, local_port, online, registered_at, last_seen_at
		FROM domains ORDER BY last_seen_at DESC
	`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	var domains []*Domain
	for rows.Next() {
		var d Domain
		err := rows.Scan(&d.ID, &d.Domain, &d.LocalIP, &d.LocalPort, &d.Online, &d.RegisteredAt, &d.LastSeenAt)
		if err != nil {
			return nil, err
		}
		domains = append(domains, &d)
	}
	return domains, nil
}

// GetOnline 获取在线域名
func GetOnline() ([]*Domain, error) {
	rows, err := db.Query(`
		SELECT id, domain, local_ip, local_port, online, registered_at, last_seen_at
		FROM domains WHERE online = 1
	`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	var domains []*Domain
	for rows.Next() {
		var d Domain
		err := rows.Scan(&d.ID, &d.Domain, &d.LocalIP, &d.LocalPort, &d.Online, &d.RegisteredAt, &d.LastSeenAt)
		if err != nil {
			return nil, err
		}
		domains = append(domains, &d)
	}
	return domains, nil
}

// SetAllOffline 设置所有域名离线
func SetAllOffline() error {
	_, err := db.Exec("UPDATE domains SET online = 0")
	return err
}

func Close() {
	if db != nil {
		db.Close()
	}
}
