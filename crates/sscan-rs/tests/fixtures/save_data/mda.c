/* Golden MDA files from saveData_writeXDR.c's own writers.
 *
 * Built with the file itself included, so the LOCAL writers are callable:
 *   gcc -I$S -I$B/include -I$B/include/os/Linux -I$B/include/compiler/gcc \
 *     mda.c $S/writeXDR.c $S/req_file.c -L$B/lib/linux-x86_64 -lca -ldbCore -lCom
 *   TZ=UTC ./a.out <dir>
 * writes <dir>/mda1d.mda and <dir>/mda2d.mda and prints the scan stamp.
 * Every channel is NULL, so no CA I/O happens: the arrays and current
 * values are the ones set here, as a completed get would leave them.
 * save_data::mda's tests replay the same calls.
 */
#include "saveData_writeXDR.c"

static epicsTimeStamp stamp = {1000000000, 123456789};

static void set_pos(SCAN *s, int i, short pxnv, short rxnv, const char *tag)
{
	int j;
	s->pxnv[i] = pxnv;
	s->rxnv[i] = rxnv;
	sprintf(s->pxpv[i], "%s:m%d", tag, i + 1);
	sprintf(s->pxds[i], "motor %d", i + 1);
	strcpy(s->pxsm[i], "LINEAR");
	strcpy(s->pxeu[i].units, "mm");
	sprintf(s->rxpv[i], "%s:m%d.RBV", tag, i + 1);
	sprintf(s->rxds[i], "readback %d", i + 1);
	strcpy(s->rxeu[i].units, rxnv == XXNV_OK ? "um" : "");
	s->pxra[i] = calloc(s->mpts, sizeof(double));
	for (j = 0; j < s->mpts; j++) s->pxra[i][j] = (i + 1) * 100.0 + j * 0.5;
}

static void set_det(SCAN *s, int i, int alloc, const char *tag)
{
	int j;
	s->dxnv[i] = XXNV_OK;
	sprintf(s->dxpv[i], "%s:det%d", tag, i + 1);
	sprintf(s->dxds[i], "detector %d", i + 1);
	strcpy(s->dxeu[i].units, "cts");
	if (alloc) {
		s->dxda[i] = calloc(s->mpts, sizeof(float));
		for (j = 0; j < s->mpts; j++) s->dxda[i][j] = (i + 1) * 10.0f + j * 0.25f;
	}
}

static SCAN *new_scan(const char *name, long mpts)
{
	SCAN *s = calloc(1, sizeof(SCAN));
	int i;
	strcpy(s->name, name);
	s->mpts = mpts;
	s->first_scan = TRUE;
	s->data = 0;
	for (i = 0; i < SCAN_NBP; i++) s->pxnv[i] = s->rxnv[i] = XXNV_NOPV;
	for (i = 0; i < SCAN_NBD; i++) s->dxnv[i] = XXNV_NOPV;
	for (i = 0; i < SCAN_NBT; i++) { s->txnv[i] = XXNV_NOPV; s->txsc[i] = 1; }
	return s;
}

static void count(SCAN *s)
{
	int i;
	s->nb_pos = s->nb_det = s->nb_trg = 0;
	for (i = 0; i < SCAN_NBP; i++) if (s->pxnv[i] == XXNV_OK || s->rxnv[i] == XXNV_OK) s->nb_pos++;
	for (i = 0; i < SCAN_NBD; i++) if (s->dxnv[i] == XXNV_OK) s->nb_det++;
	for (i = 0; i < SCAN_NBT; i++) if (s->txnv[i] == XXNV_OK) s->nb_trg++;
}

static PV_NODE *add_pv(const char *name, const char *desc, int type, long n)
{
	PV_NODE *p = calloc(1, sizeof(PV_NODE)), **tail = &list_pv;
	p->lock = epicsMutexCreate();
	strcpy(p->name, name);
	strcpy(p->desc, desc);
	p->dbr_type = type;
	p->count = n;
	p->pval = calloc(1, type == DBR_STRING ? MAX_STRING_SIZE : dbr_size_n(type, n));
	while (*tail) tail = &(*tail)->nxt;
	*tail = p;
	nb_extraPV++;
	return p;
}

static void extra_pvs(void)
{
	PV_NODE *p;
	p = add_pv("ioc:str", "a string", DBR_STRING, 1);
	strcpy((char *)p->pval, "hello");
	p = add_pv("ioc:chr", "", DBR_CTRL_CHAR, 3);
	strcpy(p->pval->cchrval.units, "ch");
	(&p->pval->cchrval.value)[0] = 'a';
	(&p->pval->cchrval.value)[1] = (dbr_char_t)0x80;
	(&p->pval->cchrval.value)[2] = 'c';
	p = add_pv("ioc:shrt", "shorts", DBR_CTRL_SHORT, 2);
	strcpy(p->pval->cshrtval.units, "s");
	(&p->pval->cshrtval.value)[0] = -1;
	(&p->pval->cshrtval.value)[1] = 2;
	p = add_pv("ioc:lng", "long", DBR_CTRL_LONG, 1);
	strcpy(p->pval->clngval.units, "counts");
	p->pval->clngval.value = 100000;
	p = add_pv("ioc:flt", "floats", DBR_CTRL_FLOAT, 2);
	strcpy(p->pval->cfltval.units, "V");
	(&p->pval->cfltval.value)[0] = 1.5f;
	(&p->pval->cfltval.value)[1] = 2.5f;
	p = add_pv("ioc:dbl", "double", DBR_CTRL_DOUBLE, 1);
	strcpy(p->pval->cdblval.units, "");
	p->pval->cdblval.value = 3.25;
}

/* proc_scan_cpt with the current values a completed get left. */
static void point(SCAN *s, long cpt)
{
	SCAN_LONG_MSG m;
	int i;
	for (i = 0; i < SCAN_NBP; i++) s->rxcv[i] = -(i + 1) - cpt * 0.125;
	for (i = 0; i < SCAN_NBD; i++) s->dxcv[i] = (float)(cpt * 1000 + i);
	memset(&m, 0, sizeof(m));
	m.pscan = s;
	m.val = cpt;
	proc_scan_cpt(&m);
}

/* proc_scan_data(0)'s file half. */
static void start(SCAN *s, int retry)
{
	count(s);
	s->cpt = 0;
	if (s->first_scan) {
		s->scan_dim = scan_getDim(s);
		reset_old_npts(s);
	}
	if (s->nxt) {
		s->nxt->first_scan = FALSE;
		s->nxt->scan_dim = s->scan_dim - 1;
		strcpy(s->nxt->fname, s->fname);
		strcpy(s->nxt->ffname, s->ffname);
	}
	s->savedSeekPos = 0;
	if (writeScanRecInProgress(s, stamp, 0)) printf("in progress %s failed\n", s->name);
	if (retry && writeScanRecInProgress(s, stamp, 1)) printf("retry %s failed\n", s->name);
}

/* proc_scan_data(1)'s file half. */
static void end(SCAN *s, long bcpt, int retry)
{
	SCAN *n;
	s->bcpt = bcpt;
	s->savedSeekPos = 0;
	if (writeScanRecCompleted(s, 0)) printf("completed %s failed\n", s->name);
	if (retry && writeScanRecCompleted(s, 1)) printf("retry %s failed\n", s->name);
	if (s->first_scan) for (n = s->nxt; n; n = n->nxt) n->first_scan = TRUE;
}

int main(int argc, char **argv)
{
	char t[MAX_STRING_SIZE];
	SCAN *s, *o, *in;
	long k, p;

	ca_context_create(ca_disable_preemptive_callback);
	save_status = STATUS_ACTIVE_OK;
	extra_pvs();
	epicsTimeToStrftime(t, MAX_STRING_SIZE, "%b %d, %Y %H:%M:%S.%06f", &stamp);
	printf("stamp '%s'\n", t);

	/* 1D: two positioners (one readback-only), a trigger, three
	 * detectors of which one has no array yet, stopped after 3 of 5. */
	s = new_scan("ioc:scan1", 10);
	s->counter = 7;
	s->npts = 5;
	set_pos(s, 0, XXNV_OK, XXNV_OK, "ioc");
	set_pos(s, 2, XXNV_BADPV, XXNV_OK, "ioc");
	set_det(s, 0, 1, "ioc");
	set_det(s, 5, 1, "ioc");
	set_det(s, 69, 0, "ioc");
	s->txnv[1] = XXNV_OK;
	strcpy(s->txpv[1], "ioc:scanH.EXSC");
	s->txcd[1] = 1.0f;
	strcpy(s->fname, "t_0007.mda");
	sprintf(s->ffname, "%s/mda1d.mda", argv[1]);
	start(s, 0);
	for (p = 1; p <= 3; p++) point(s, p);
	end(s, 3, 0);

	/* 2D: the inner scan's npts drops from 4 to 3 on the last outer
	 * point; the inner start and the outer end are each retried once. */
	o = new_scan("ioc:scan2", 3);
	in = new_scan("ioc:scan1", 4);
	o->nxt = in;
	o->counter = 8;
	o->npts = 3;
	set_pos(o, 1, XXNV_OK, XXNV_NOPV, "out");
	set_det(o, 2, 1, "out");
	in->npts = 4;
	set_pos(in, 0, XXNV_OK, XXNV_OK, "in");
	set_det(in, 0, 1, "in");
	set_det(in, 1, 1, "in");
	strcpy(o->fname, "t_0008.mda");
	sprintf(o->ffname, "%s/mda2d.mda", argv[1]);
	start(o, 0);
	for (k = 1; k <= 3; k++) {
		if (k == 3) in->npts = 3;
		start(in, k == 2);
		for (p = 1; p <= in->npts; p++) point(in, p);
		end(in, in->npts, 0);
		point(o, k);
	}
	end(o, 3, 1);
	return 0;
}
