package main

import (
	"encoding/json"
	"fmt"
	"os"
	"strings"

	"github.com/charmbracelet/huh"
)

type Config struct {
	Owner         string   `json:"owner"`
	AllRepos      bool     `json:"all_repos"`
	Repos         []string `json:"repos"`
	Exclude       []string `json:"exclude"`
	SyncPRs       bool     `json:"sync_prs"`
	SyncEvents    bool     `json:"sync_events"`
	SyncBranches  []string `json:"sync_branches"`
	DBPath        string   `json:"db_path"`
	StagingFolder string   `json:"staging_folder"`
}

func splitCSV(s string) []string {
	parts := strings.Split(s, ",")
	out := make([]string, 0, len(parts))
	for _, p := range parts {
		p = strings.TrimSpace(p)
		if p != "" {
			out = append(out, p)
		}
	}
	return out
}

func main() {
	var (
		owner         string
		syncMode      string = "all"
		specificRepos string
		excludeRepos  string
		branches      string = "main"
		syncPRs       bool   = true
		syncEvents    bool   = true
		dbPath        string = "~/.local/share/ghcache/gh.db"
		stagingFolder string = "~/.local/share/ghcache/repos"
	)

	form := huh.NewForm(
		huh.NewGroup(
			huh.NewInput().
				Title("GitHub username or org").
				Description("Syncs all repos under this account. Run: gh api user --jq '.login'").
				Placeholder("yourname").
				Value(&owner).
				Validate(func(s string) error {
					if strings.TrimSpace(s) == "" {
						return fmt.Errorf("owner is required")
					}
					return nil
				}),

			huh.NewSelect[string]().
				Title("Repo scope").
				Options(
					huh.NewOption("All repos in account", "all"),
					huh.NewOption("Specific repos only", "specific"),
				).
				Value(&syncMode),
		),

		huh.NewGroup(
			huh.NewInput().
				Title("Repo names").
				Description("Comma-separated list, e.g: backend,frontend,infra").
				Placeholder("repo1,repo2").
				Value(&specificRepos),
		).WithHideFunc(func() bool {
			return syncMode == "all"
		}),

		huh.NewGroup(
			huh.NewInput().
				Title("Exclude repos (optional)").
				Description("Comma-separated repo names to skip").
				Placeholder("scratch,archived-thing").
				Value(&excludeRepos),
		).WithHideFunc(func() bool {
			return syncMode == "specific"
		}),

		huh.NewGroup(
			huh.NewInput().
				Title("Branches to track").
				Description("Comma-separated, glob * supported. e.g: main,staging,release/*").
				Value(&branches),

			huh.NewConfirm().
				Title("Sync pull requests?").
				Value(&syncPRs),

			huh.NewConfirm().
				Title("Sync repo events? (drives targeted PR sync)").
				Value(&syncEvents),

			huh.NewInput().
				Title("Database path").
				Value(&dbPath),

			huh.NewInput().
				Title("Staging folder for git checkouts (optional)").
				Description("Leave empty to skip checkout support").
				Placeholder("~/src/staging").
				Value(&stagingFolder),
		),
	).WithOutput(os.Stderr)

	err := form.Run()
	if err != nil {
		os.Exit(1)
	}

	cfg := Config{
		Owner:         strings.TrimSpace(owner),
		AllRepos:      syncMode == "all",
		Repos:         splitCSV(specificRepos),
		Exclude:       splitCSV(excludeRepos),
		SyncPRs:       syncPRs,
		SyncEvents:    syncEvents,
		SyncBranches:  splitCSV(branches),
		DBPath:        strings.TrimSpace(dbPath),
		StagingFolder: strings.TrimSpace(stagingFolder),
	}

	out, err := json.MarshalIndent(cfg, "", "  ")
	if err != nil {
		fmt.Fprintf(os.Stderr, "json marshal error: %v\n", err)
		os.Exit(1)
	}

	fmt.Println(string(out))
}
