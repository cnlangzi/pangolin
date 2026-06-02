package config

import (
	"os"

	"gopkg.in/yaml.v3"
)

type Config struct {
	Server  ServerConfig     `yaml:"server"`
	Domains map[string]DomainConfig `yaml:"domains"`
	Admin   AdminConfig      `yaml:"admin"`
	Cache   CacheConfig     `yaml:"cache"`
	Cert    CertConfig      `yaml:"cert"`
	Log     LogConfig       `yaml:"log"`
}

type ServerConfig struct {
	Port    int    `yaml:"port"`     // HTTP监听端口
	TLSPort int    `yaml:"tls_port"` // HTTPS监听端口
	WSPath  string `yaml:"ws_path"`  // WebSocket路径
	Token   string `yaml:"token"`   // 认证token
}

type DomainConfig struct {
	Domain    string `yaml:"domain"`
	LocalIP   string `yaml:"local_ip"`
	LocalPort int    `yaml:"local_port"`
	Enabled   bool   `yaml:"enabled"`
}

type AdminConfig struct {
	Username string `yaml:"username"`
	Password string `yaml:"password"`
}

type CacheConfig struct {
	Enabled bool   `yaml:"enabled"`
	Dir     string `yaml:"dir"`
}

type CertConfig struct {
	Email   string `yaml:"email"`
	CertDir string `yaml:"cert_dir"`
}

type LogConfig struct {
	Level string `yaml:"level"`
	File  string `yaml:"file"`
}

func Load(path string) (*Config, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	
	cfg := &Config{}
	if err := yaml.Unmarshal(data, cfg); err != nil {
		return nil, err
	}
	
	// 设置默认值
	if cfg.Server.Port == 0 {
		cfg.Server.Port = 8080
	}
	if cfg.Server.TLSPort == 0 {
		cfg.Server.TLSPort = 8443
	}
	if cfg.Server.WSPath == "" {
		cfg.Server.WSPath = "/tunnel"
	}
	if cfg.Cache.Dir == "" {
		cfg.Cache.Dir = "./cache"
	}
	
	return cfg, nil
}
