package version

import (
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"sync"
	"time"
)

// VersionInfo holds the version information from npm registry
type VersionInfo struct {
	Version     string
	LastUpdated time.Time
}

var (
	versionInfo *VersionInfo
	versionMu   sync.RWMutex
)

const versionCacheDuration = 30 * time.Minute

// GetCommandCodeVersion fetches the latest version from npm registry
// It caches the result for 30 minutes to avoid excessive API calls
func GetCommandCodeVersion() string {
	versionMu.RLock()
	info := versionInfo
	versionMu.RUnlock()

	// Return cached version if still valid
	if info != nil && time.Since(info.LastUpdated) < versionCacheDuration {
		return info.Version
	}

	// Fetch new version
	newInfo, err := fetchVersionFromNPM()
	if err != nil {
		// Return cached version on error, or default
		versionMu.RLock()
		defer versionMu.RUnlock()
		if versionInfo != nil {
			return versionInfo.Version
		}
		return "unknown"
	}

	// Update cache
	versionMu.Lock()
	versionInfo = newInfo
	versionMu.Unlock()

	return newInfo.Version
}

// fetchVersionFromNPM fetches the latest version from npm registry
func fetchVersionFromNPM() (*VersionInfo, error) {
	resp, err := http.Get("https://registry.npmjs.org/command-code/latest")
	if err != nil {
		return nil, fmt.Errorf("failed to fetch version: %w", err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		body, _ := io.ReadAll(resp.Body)
		return nil, fmt.Errorf("npm registry returned %d: %s", resp.StatusCode, string(body))
	}

	var result map[string]any
	if err := json.NewDecoder(resp.Body).Decode(&result); err != nil {
		return nil, fmt.Errorf("failed to parse version: %w", err)
	}

	version, ok := result["version"].(string)
	if !ok {
		return nil, fmt.Errorf("version field not found or invalid")
	}

	return &VersionInfo{
		Version:     version,
		LastUpdated: time.Now(),
	}, nil
}

// init fetches the version on startup (in background)
func init() {
	go func() {
		// Initial fetch - ignore error, will retry on first use
		_, _ = fetchVersionFromNPM()
	}()
}
