package cosmos

import (
	"context"
	"net/http"
	"testing"

	"azemu-sdk-go/internal/emu"

	"github.com/Azure/azure-sdk-for-go/sdk/data/azcosmos"
)

func TestDatabase_CreateTwiceIsConflict409(t *testing.T) {
	ctx := context.Background()
	c := newClient(t)
	id := emu.Name("dup")
	if _, err := c.CreateDatabase(ctx, azcosmos.DatabaseProperties{ID: id}, nil); err != nil {
		t.Fatal(err)
	}
	_, err := c.CreateDatabase(ctx, azcosmos.DatabaseProperties{ID: id}, nil)
	status(t, err, http.StatusConflict)
}

func TestDatabase_ReadReturnsIdAndETagAndDeleteMakesItNotFound404(t *testing.T) {
	ctx := context.Background()
	c := newClient(t)
	db := newDB(t, c)
	r, err := db.Read(ctx, nil)
	if err != nil || r.DatabaseProperties.ID != db.ID() || r.DatabaseProperties.ETag == nil || r.DatabaseProperties.ResourceID == "" {
		t.Fatalf("read: %v %+v", err, r.DatabaseProperties)
	}
	if _, err := db.Delete(ctx, nil); err != nil {
		t.Fatal(err)
	}
	_, err = db.Read(ctx, nil)
	status(t, err, http.StatusNotFound)
}

func TestDatabase_QueryDatabasesWithAParameter(t *testing.T) {
	ctx := context.Background()
	c := newClient(t)
	db := newDB(t, c)
	newDB(t, c)
	found := 0
	for p := c.NewQueryDatabasesPager("SELECT * FROM d WHERE d.id = @id", &azcosmos.QueryDatabasesOptions{
		QueryParameters: []azcosmos.QueryParameter{{Name: "@id", Value: db.ID()}}}); p.More(); {
		pg, err := p.NextPage(ctx)
		if err != nil {
			t.Fatal(err)
		}
		found += len(pg.Databases)
	}
	if found != 1 {
		t.Fatalf("found %d", found)
	}
}

func TestContainer_CreateReadReportsPartitionKeyIndexingAndTTL(t *testing.T) {
	ctx := context.Background()
	c := newClient(t)
	ct := newContainerWith(t, c, azcosmos.ContainerProperties{
		PartitionKeyDefinition: azcosmos.PartitionKeyDefinition{Paths: []string{"/tenant"}, Kind: azcosmos.PartitionKeyKindHash, Version: 2},
		DefaultTimeToLive:      ptr(int32(3600)),
	}, nil)
	r, err := ct.Read(ctx, nil)
	if err != nil {
		t.Fatal(err)
	}
	p := r.ContainerProperties
	if len(p.PartitionKeyDefinition.Paths) != 1 || p.PartitionKeyDefinition.Paths[0] != "/tenant" || p.PartitionKeyDefinition.Version != 2 {
		t.Fatalf("partition key %+v", p.PartitionKeyDefinition)
	}
	if p.DefaultTimeToLive == nil || *p.DefaultTimeToLive != 3600 {
		t.Fatalf("ttl %v", p.DefaultTimeToLive)
	}
	if p.IndexingPolicy == nil || !p.IndexingPolicy.Automatic {
		t.Fatalf("indexing policy %+v", p.IndexingPolicy)
	}
	if p.ETag == nil || p.ResourceID == "" {
		t.Fatal("etag/rid missing")
	}
}

func TestContainer_CreateTwiceIsConflict409(t *testing.T) {
	ctx := context.Background()
	c := newClient(t)
	db := newDB(t, c)
	props := azcosmos.ContainerProperties{ID: "same", PartitionKeyDefinition: azcosmos.PartitionKeyDefinition{Paths: []string{"/pk"}}}
	if _, err := db.CreateContainer(ctx, props, nil); err != nil {
		t.Fatal(err)
	}
	_, err := db.CreateContainer(ctx, props, nil)
	status(t, err, http.StatusConflict)
}

func TestContainer_ReplaceChangesDefaultTTLAndIndexingPolicy(t *testing.T) {
	ctx := context.Background()
	c := newClient(t)
	ct := newContainer(t, c)
	r, _ := ct.Read(ctx, nil)
	p := *r.ContainerProperties
	p.DefaultTimeToLive = ptr(int32(60))
	p.IndexingPolicy = &azcosmos.IndexingPolicy{Automatic: true, IndexingMode: "consistent", // the SDK constant is "Consistent", see the next test
		IncludedPaths: []azcosmos.IncludedPath{{Path: "/name/?"}}, ExcludedPaths: []azcosmos.ExcludedPath{{Path: "/*"}}}
	if _, err := ct.Replace(ctx, p, nil); err != nil {
		t.Fatal(err)
	}
	r2, _ := ct.Read(ctx, nil)
	if *r2.ContainerProperties.DefaultTimeToLive != 60 || len(r2.ContainerProperties.IndexingPolicy.IncludedPaths) != 1 {
		t.Fatalf("after replace %+v", r2.ContainerProperties)
	}
}

func TestContainer_QueryContainersListsTheDatabasesContainers(t *testing.T) {
	ctx := context.Background()
	c := newClient(t)
	db := newDB(t, c)
	for _, id := range []string{"c1", "c2", "c3"} {
		db.CreateContainer(ctx, azcosmos.ContainerProperties{ID: id, PartitionKeyDefinition: azcosmos.PartitionKeyDefinition{Paths: []string{"/pk"}}}, nil)
	}
	n := 0
	for p := db.NewQueryContainersPager("SELECT * FROM c", nil); p.More(); {
		pg, err := p.NextPage(ctx)
		if err != nil {
			t.Fatal(err)
		}
		n += len(pg.Containers)
	}
	if n != 3 {
		t.Fatalf("%d containers", n)
	}
}

func TestContainer_DeleteThenReadIsNotFound404(t *testing.T) {
	ctx := context.Background()
	ct := newContainer(t, newClient(t))
	if _, err := ct.Delete(ctx, nil); err != nil {
		t.Fatal(err)
	}
	_, err := ct.Read(ctx, nil)
	status(t, err, http.StatusNotFound)
}

func TestThroughput_ManualOnAContainerReadsBackAndReplaces(t *testing.T) {
	ctx := context.Background()
	c := newClient(t)
	tp := azcosmos.NewManualThroughputProperties(400)
	ct := newContainerWith(t, c, azcosmos.ContainerProperties{PartitionKeyDefinition: azcosmos.PartitionKeyDefinition{Paths: []string{"/pk"}}}, &tp)
	r, err := ct.ReadThroughput(ctx, nil)
	if err != nil {
		t.Fatal(err)
	}
	if v, ok := r.ThroughputProperties.ManualThroughput(); !ok || v != 400 {
		t.Fatalf("throughput %v %v", v, ok)
	}
	if _, err := ct.ReplaceThroughput(ctx, azcosmos.NewManualThroughputProperties(1000), nil); err != nil {
		t.Fatal(err)
	}
	r, _ = ct.ReadThroughput(ctx, nil)
	if v, _ := r.ThroughputProperties.ManualThroughput(); v != 1000 {
		t.Fatalf("after replace %d", v)
	}
}

func TestThroughput_AutoscaleOnAContainerReportsMaxThroughput(t *testing.T) {
	ctx := context.Background()
	c := newClient(t)
	tp := azcosmos.NewAutoscaleThroughputProperties(4000)
	ct := newContainerWith(t, c, azcosmos.ContainerProperties{PartitionKeyDefinition: azcosmos.PartitionKeyDefinition{Paths: []string{"/pk"}}}, &tp)
	r, err := ct.ReadThroughput(ctx, nil)
	if err != nil {
		t.Fatal(err)
	}
	if v, ok := r.ThroughputProperties.AutoscaleMaxThroughput(); !ok || v != 4000 {
		t.Fatalf("autoscale max %v %v", v, ok)
	}
}

func TestThroughput_OnADatabaseIsSharedAndContainerHasNoOfferOfItsOwn(t *testing.T) {
	ctx := context.Background()
	c := newClient(t)
	id := emu.Name("shared")
	_, err := c.CreateDatabase(ctx, azcosmos.DatabaseProperties{ID: id}, &azcosmos.CreateDatabaseOptions{ThroughputProperties: ptr(azcosmos.NewManualThroughputProperties(800))})
	if err != nil {
		t.Fatal(err)
	}
	db, _ := c.NewDatabase(id)
	r, err := db.ReadThroughput(ctx, nil)
	if err != nil {
		t.Fatal(err)
	}
	if v, ok := r.ThroughputProperties.ManualThroughput(); !ok || v != 800 {
		t.Fatalf("db throughput %v", v)
	}
}

func TestContainer_CreateWithTheSdkConstantsForIndexingModeIsAccepted(t *testing.T) {
	ctx := context.Background()
	c := newClient(t)
	db := newDB(t, c)
	for i, m := range []azcosmos.IndexingMode{azcosmos.IndexingModeConsistent, azcosmos.IndexingModeNone} {
		_, err := db.CreateContainer(ctx, azcosmos.ContainerProperties{ID: emu.Name("im") + string(rune('a'+i)),
			PartitionKeyDefinition: azcosmos.PartitionKeyDefinition{Paths: []string{"/pk"}},
			IndexingPolicy:         &azcosmos.IndexingPolicy{Automatic: m == azcosmos.IndexingModeConsistent, IndexingMode: m}}, nil)
		if err != nil {
			t.Fatalf("indexing mode %q: %v", m, err)
		}
	}
}
