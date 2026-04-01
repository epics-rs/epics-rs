/*
 * golden_gen.c — Generate golden test values from the original C table record math.
 *
 * Extracts the pure math functions from tableRecord.c and runs them with
 * specific inputs to produce reference values for Rust golden tests.
 *
 * Compile: cc -o golden_gen golden_gen.c -lm
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <math.h>

/* Constants from tableRecord.c */
#define SMALL 1.e-6
#define LARGE 1.e9
#define X 0
#define Y 1
#define Z 2
#define AX_6 0
#define AY_6 1
#define AZ_6 2
#define X_6  3
#define Y_6  4
#define Z_6  5
#define M0X 0
#define M0Y 1
#define M1Y 2
#define M2X 3
#define M2Y 4
#define M2Z 5

#define VERSION 5.14

double D2R;

/* Geometry enum */
#define GEOM_SRI     0
#define GEOM_GEOCARS 1
#define GEOM_NEWPORT 2
#define GEOM_PNC     3

/* Minimal table structure with just the fields the math functions need */
struct table {
    int geom;
    double lx, lz;
    double sx, sy, sz;
    double rx, ry, rz;
    double yang;
    double torad;

    double pp0[3], pp1[3], pp2[3];
    double ppo0[3], ppo1[3], ppo2[3];
    double a[3][3];
    double b[3][3];

    double ax0[6];

    /* link status: 1 = connected */
    int can_rw_drive[6];
};

/* Forward declarations */
static void InitGeometry(struct table *t);
static void MakeRotationMatrix(struct table *t, double *u);
static void MotorToUser(struct table *t, double *m, double *u);
static void UserToMotor(struct table *t, double *u, double *m);
static void NaiveMotorToPivotPointVector(struct table *t, double *m,
    double *q0, double *q1, double *q2);
static void MotorToPivotPointVector(struct table *t, double *m,
    double *q0, double *q1, double *q2);
static void PivotPointVectorToLocalUserAngles(struct table *t,
    double *q0, double *q1, double *q2, double *u);
static void MotorToLocalUserAngles(struct table *t, double *m, double *u);
static void LocalUserToPivotPointVector(struct table *t, double *u,
    double *pp0, double *pp1, double *pp2);
static void PivotPointVectorToMotor(struct table *t,
    double *pp0, double *pp1, double *pp2, double *m, double *u);
static void LabToLocal(struct table *t, double *lab, double *local);
static void LocalToLab(struct table *t, double *local, double *lab);
static void RotY(double *in_, double *out, double a);

/* ---- Implementation (extracted from tableRecord.c) ---- */

static void InitGeometry(struct table *t)
{
    double *pp0=t->pp0, *pp1=t->pp1, *pp2=t->pp2;
    double *ppo0=t->ppo0, *ppo1=t->ppo1, *ppo2=t->ppo2;
    double **bb_dummy; /* we use t->b directly */
    double sx=t->sx, sy=t->sy, sz=t->sz;
    double lx=t->lx, lz=t->lz;
    double fx, fy, fz;
    double av, bv, cv, dv, ev, fv, gv, hv, iv, det;

    fx = t->rx + sx;
    fy = t->ry + sy;
    fz = t->rz + sz;

    switch (t->geom) {
    case GEOM_GEOCARS:
        pp0[X]= -fx;       pp0[Y]= -fy;  pp0[Z]= lz/2 - fz;
        pp1[X]= lx - fx;   pp1[Y]= -fy;  pp1[Z]= lz - fz;
        pp2[X]= lx - fx;   pp2[Y]= -fy;  pp2[Z]= -fz;
        break;
    case GEOM_NEWPORT:
        pp0[X]= lx - fx;   pp0[Y]= -fy;  pp0[Z]= -fz;
        pp1[X]= -fx;       pp1[Y]= -fy;  pp1[Z]= lz/2 - fz;
        pp2[X]= lx - fx;   pp2[Y]= -fy;  pp2[Z]= lz - fz;
        break;
    case GEOM_PNC:
        pp0[X]= -fx;       pp0[Y]= -fy;  pp0[Z]= -fz;
        pp1[X]= lx - fx;   pp1[Y]= -fy;  pp1[Z]= -fz;
        pp2[X]= lx/2 - fx; pp2[Y]= -fy;  pp2[Z]= lz - fz;
        break;
    case GEOM_SRI:
    default:
        pp0[X]= lx - fx;   pp0[Y]= -fy;  pp0[Z]= -fz;
        pp1[X]= -fx;       pp1[Y]= -fy;  pp1[Z]= -fz;
        pp2[X]= lx/2 - fx; pp2[Y]= -fy;  pp2[Z]= lz - fz;
        break;
    }
    memcpy(ppo0, pp0, 3*sizeof(double));
    memcpy(ppo1, pp1, 3*sizeof(double));
    memcpy(ppo2, pp2, 3*sizeof(double));

    av = ppo1[X]-ppo0[X]; bv = ppo1[Y]-ppo0[Y]; cv = ppo1[Z]-ppo0[Z];
    dv = ppo2[X]-ppo1[X]; ev = ppo2[Y]-ppo1[Y]; fv = ppo2[Z]-ppo1[Z];
    gv = bv*fv - cv*ev;   hv = cv*dv - av*fv;   iv = av*ev - bv*dv;

    det = av*(ev*iv-hv*fv) + bv*(fv*gv-iv*dv) + cv*(dv*hv-gv*ev);
    t->b[0][0]=(ev*iv-fv*hv)/det; t->b[0][1]=(cv*hv-bv*iv)/det; t->b[0][2]=(bv*fv-cv*ev)/det;
    t->b[1][0]=(fv*gv-dv*iv)/det; t->b[1][1]=(av*iv-cv*gv)/det; t->b[1][2]=(cv*dv-av*fv)/det;
    t->b[2][0]=(dv*hv-ev*gv)/det; t->b[2][1]=(bv*gv-av*hv)/det; t->b[2][2]=(av*ev-bv*dv)/det;
}

static void MakeRotationMatrix(struct table *t, double *u)
{
    double cx, cy, cz, sx_, sy_, sz_;
    cx = cos(t->torad * u[AX_6]); sx_ = sin(t->torad * u[AX_6]);
    cy = cos(t->torad * u[AY_6]); sy_ = sin(t->torad * u[AY_6]);
    cz = cos(t->torad * u[AZ_6]); sz_ = sin(t->torad * u[AZ_6]);

    t->a[0][0] = cy*cz;              t->a[0][1] = cy*sz_;             t->a[0][2] = -sy_;
    t->a[1][0] = sx_*sy_*cz - cx*sz_; t->a[1][1] = sx_*sy_*sz_ + cx*cz; t->a[1][2] = sx_*cy;
    t->a[2][0] = cx*sy_*cz + sx_*sz_; t->a[2][1] = cx*sy_*sz_ - sx_*cz; t->a[2][2] = cx*cy;
}

static void RotY(double *in_, double *out, double a)
{
    int i; double in[6];
    for (i=0;i<6;i++) in[i]=in_[i];
    out[X_6]  = in[X_6]*cos(a)  + in[Z_6]*sin(a);
    out[AX_6] = in[AX_6]*cos(a) + in[AZ_6]*sin(a);
    out[Z_6]  = in[X_6]*(-sin(a)) + in[Z_6]*cos(a);
    out[AZ_6] = in[AX_6]*(-sin(a)) + in[AZ_6]*cos(a);
    out[Y_6]  = in[Y_6];
    out[AY_6] = in[AY_6];
}

static void LabToLocal(struct table *t, double *lab, double *local)
{ RotY(lab, local, t->yang*D2R); }

static void LocalToLab(struct table *t, double *local, double *lab)
{ RotY(local, lab, -t->yang*D2R); }

static void NaiveMotorToPivotPointVector(struct table *t, double *m,
    double *q0, double *q1, double *q2)
{
    double *p0=t->ppo0, *p1=t->ppo1, *p2=t->ppo2;
    switch (t->geom) {
    case GEOM_SRI: case GEOM_PNC: case GEOM_GEOCARS: default:
        q0[X] = p0[X]+m[M0X]; q0[Y] = p0[Y]+m[M0Y];
        q1[Y] = p1[Y]+m[M1Y];
        q2[X] = p2[X]+m[M2X]; q2[Y] = p2[Y]+m[M2Y]; q2[Z] = p2[Z]+m[M2Z];
        break;
    case GEOM_NEWPORT:
        MakeRotationMatrix(t, &t->ax0[0]); /* uses current user angles */
        {
            double norm[3];
            norm[X]=t->a[X][Y]; norm[Y]=t->a[Y][Y]; norm[Z]=t->a[Z][Y];
            q0[X]=p0[X]+m[M0X]+norm[X]*m[M0Y]; q0[Y]=p0[Y]+norm[Y]*m[M0Y];
            q1[Y]=p1[Y]+norm[Y]*m[M1Y];
            q2[X]=p2[X]+m[M2X]+norm[X]*m[M2Y]; q2[Y]=p2[Y]+norm[Y]*m[M2Y];
            q2[Z]=p2[Z]+m[M2Z]+norm[Z]*m[M2Y];
        }
        break;
    }
}

static void MotorToPivotPointVector(struct table *t, double *m,
    double *q0, double *q1, double *q2)
{
    int i;
    double *p0=t->ppo0, *p1=t->ppo1, *p2=t->ppo2;
    double q1z_m, s, tt, p10p20, p10p10, alpha;
    double dx, dy, dz, d0x, d0y, d0z;

    NaiveMotorToPivotPointVector(t, m, q0, q1, q2);

    d0x=p2[X]-p0[X]; d0y=p2[Y]-p0[Y]; d0z=p2[Z]-p0[Z];
    dx=q2[X]-q0[X]; dy=q2[Y]-q0[Y]; dz=q2[Z]-q0[Z];

    switch(t->geom) {
    case GEOM_GEOCARS:
        q0[Z] = q2[Z] + sqrt(d0x*d0x+d0y*d0y+d0z*d0z-(dx*dx+dy*dy));
        break;
    default:
        q0[Z] = q2[Z] - sqrt(d0x*d0x+d0y*d0y+d0z*d0z-(dx*dx+dy*dy));
        break;
    }

    for(i=X,p10p20=0;i<=Z;i++) p10p20+=(p1[i]-p0[i])*(p2[i]-p0[i]);
    s = -(q0[Z]-q2[Z])/(q0[X]-q2[X]);
    tt = (-p10p20+q0[X]*(q0[X]-q2[X])+(q0[Y]-q1[Y])*(q0[Y]-q2[Y])+
         q0[Z]*(q0[Z]-q2[Z]))/(q0[X]-q2[X]);
    for(i=X,p10p10=0;i<=Z;i++) p10p10+=(p1[i]-p0[i])*(p1[i]-p0[i]);
    alpha=sqrt((2*s*tt-2*s*q0[X]-2*q0[Z])*(2*s*tt-2*s*q0[X]-2*q0[Z])-
        4*(1+s*s)*(tt*tt-p10p10-2*tt*q0[X]+q0[X]*q0[X]+q0[Y]*q0[Y]+
        q0[Z]*q0[Z]-2*q0[Y]*q1[Y]+q1[Y]*q1[Y]));
    q1[Z]=(-2*s*tt+2*s*q0[X]+2*q0[Z]+alpha)/(2*(1+s*s));
    q1z_m=(-2*s*tt+2*s*q0[X]+2*q0[Z]-alpha)/(2*(1+s*s));
    if(fabs(q1[Z]-p1[Z])>fabs(q1z_m-p1[Z])) q1[Z]=q1z_m;
    q1[X]=s*q1[Z]+tt;
}

static void PivotPointVectorToLocalUserAngles(struct table *t,
    double *q0, double *q1, double *q2, double *u)
{
    double av,bv,cv,dv,ev,fv,gv,hv,iv;
    double jp[3],kp[3];

    av=q1[X]-q0[X]; bv=q1[Y]-q0[Y]; cv=q1[Z]-q0[Z];
    dv=q2[X]-q1[X]; ev=q2[Y]-q1[Y]; fv=q2[Z]-q1[Z];
    gv=bv*fv-cv*ev; hv=cv*dv-av*fv; iv=av*ev-bv*dv;

    jp[X]=t->b[1][0]*av+t->b[1][1]*dv+t->b[1][2]*gv;
    kp[X]=t->b[2][0]*av+t->b[2][1]*dv+t->b[2][2]*gv;
    kp[Y]=t->b[2][0]*bv+t->b[2][1]*ev+t->b[2][2]*hv;

    u[AY_6]=asin(-kp[X]);
    u[AX_6]=asin(kp[Y]/cos(u[AY_6]))/t->torad;
    u[AZ_6]=asin(jp[X]/cos(u[AY_6]))/t->torad;
    u[AY_6]/=t->torad;
}

/* Newport motor-to-angles (complex Mathematica-derived formulas) */
static void MotorToLocalUserAngles(struct table *t, double *m, double *u)
{
    double *p0=t->ppo0, *p1=t->ppo1, *p2=t->ppo2;
    double p10x=p1[X]-p0[X], p10y=p1[Y]-p0[Y], p10z=p1[Z]-p0[Z];
    double p20x=p2[X]-p0[X], p20y=p2[Y]-p0[Y], p20z=p2[Z]-p0[Z];
    double p02x=p0[X]-p2[X], p02y=p0[Y]-p2[Y], p02z=p0[Z]-p2[Z];
    double p02x_2=p02x*p02x, p02y_2=p02y*p02y, p02z_2=p02z*p02z;
    double L0=m[M0Y], L1=m[M1Y], L2=m[M2Y];
    double L10=m[M1Y]-m[M0Y], L20=m[M2Y]-m[M0Y], L02=m[M0Y]-m[M2Y];
    double L02_2=L02*L02;
    double Ryx,Ryy,Ryz,Ryx_2,Ryy_2,Ryz_2;
    double Rxx,Rxy,Rxz;
    double n0x=m[M0X],n2x=m[M2X],n02x=n0x-n2x,n02x_2=n02x*n02x;
    double aa,bb_,cc,tmp,tmp1,npx,npy,npz;

    npx=p10y*p20z-p10z*p20y-p20z*L10+p10z*L20;
    npy=p10z*p20x-p10x*p20z;
    npz=p10x*p20y-p10y*p20x+p20x*L10-p10x*L20;
    Ryy=npy/sqrt(npx*npx+npy*npy+npz*npz);
    Ryy_2=Ryy*Ryy;

    Ryx=(p1[Z]*p2[Y]-p1[Y]*p2[Z]+p0[Y]*(p1[Z]-p2[Z])*(-1+Ryy)-
        (p1[Z]*(L0-L2+p2[Y])-(L0-L1+p1[Y])*p2[Z])*Ryy+
        p0[Z]*(p1[Y]-p2[Y]+(L1-L2-p1[Y]+p2[Y])*Ryy))/
        (p0[Z]*(p1[X]-p2[X])+p1[Z]*p2[X]-p1[X]*p2[Z]+p0[X]*(-p1[Z]+p2[Z]));
    Ryx_2=Ryx*Ryx;

    Ryz=(p1[Y]*p2[X]-p1[X]*p2[Y]-p0[Y]*(p1[X]-p2[X])*(-1+Ryy)+
        (L0*p1[X]-L2*p1[X]-L0*p2[X]+L1*p2[X]-p1[Y]*p2[X]+p1[X]*p2[Y])*Ryy+
        p0[X]*(-p1[Y]+p2[Y]+(-L1+L2+p1[Y]-p2[Y])*Ryy))/
        (p0[Z]*(p1[X]-p2[X])+p1[Z]*p2[X]-p1[X]*p2[Z]+p0[X]*(-p1[Z]+p2[Z]));
    Ryz_2=Ryz*Ryz;

    u[Y_6]=(-(p0[X]*p1[Z]*p2[Y])+p0[X]*p1[Y]*p2[Z]-
        p0[Y]*(p1[Z]*p2[X]-p1[X]*p2[Z])*(-1+Ryy)-L2*p0[X]*p1[Z]*Ryy+
        L0*p1[Z]*p2[X]*Ryy+p0[X]*p1[Z]*p2[Y]*Ryy+L1*p0[X]*p2[Z]*Ryy-
        L0*p1[X]*p2[Z]*Ryy-p0[X]*p1[Y]*p2[Z]*Ryy+
        p0[Z]*(p1[Y]*p2[X]*(-1+Ryy)-(L1*p2[X]+p1[X]*p2[Y])*Ryy+
        p1[X]*(p2[Y]+L2*Ryy)))/
        (p0[Z]*(p1[X]-p2[X])+p1[Z]*p2[X]-p1[X]*p2[Z]+p0[X]*(-p1[Z]+p2[Z]));

    aa=(n02x+p02x)*(L02*Ryx*Ryy-p02y*Ryx*Ryy-p02z*Ryx*Ryz+p02x*(Ryy_2+Ryz_2));
    bb_=-p02x_2*Ryx_2+p02y_2*Ryx_2+p02z_2*Ryx_2-2*p02x*p02y*Ryx*Ryy+p02z_2*Ryy_2-
        2*p02z*(p02x*Ryx+p02y*Ryy)*Ryz+p02y_2*Ryz_2+L02_2*(Ryx_2+Ryz_2)-
        n02x_2*(Ryx_2+Ryy_2+Ryz_2)-2*n02x*p02x*(Ryx_2+Ryy_2+Ryz_2)-
        2*L02*(-Ryy*(p02x*Ryx+p02z*Ryz)+p02y*(Ryx_2+Ryz_2));
    cc=L02_2*Ryx_2-2*L02*p02y*Ryx_2+p02y_2*Ryx_2+p02z_2*Ryx_2+
        2*L02*p02x*Ryx*Ryy-2*p02x*p02y*Ryx*Ryy+p02x_2*Ryy_2+p02z_2*Ryy_2-
        2*p02z*(p02x*Ryx+(-L02+p02y)*Ryy)*Ryz+(p02x_2+(L02-p02y)*(L02-p02y))*Ryz_2;

    Rxx=(aa-(p02z*Ryy+(L02-p02y)*Ryz)*sqrt(bb_))/cc;
    tmp=(aa+(p02z*Ryy+(L02-p02y)*Ryz)*sqrt(bb_))/cc;
    if(fabs(tmp-1)<fabs(Rxx-1)) Rxx=tmp;

    aa=(n02x+p02x)*(p02z*(Ryx_2+Ryy_2)-(p02x*Ryx+(-L02+p02y)*Ryy)*Ryz);
    Rxz=(aa+(L02*Ryx-p02y*Ryx+p02x*Ryy)*sqrt(bb_))/cc;

    aa=-(n02x+p02x)*(Ryx*(L02*Ryx-p02y*Ryx+p02x*Ryy)+
        p02z*Ryy*Ryz+(L02-p02y)*Ryz_2);
    Rxy=(aa+(p02z*Ryx-p02x*Ryz)*sqrt(bb_))/cc;
    tmp=(aa-(p02z*Ryx-p02x*Ryz)*sqrt(bb_))/cc;
    tmp1=sqrt(1-(Rxx*Rxx+Rxz*Rxz));
    if(fabs(fabs(fabs(tmp)-tmp1)-fabs(fabs(Rxy)-tmp1))>1e-6) {
        if(fabs(fabs(tmp)-tmp1)<fabs(fabs(Rxy)-tmp1)) Rxy=tmp;
    }

    u[AY_6]=asin(-Rxz);
    u[AX_6]=asin(Ryz/cos(u[AY_6]))/t->torad;
    u[AZ_6]=asin(Rxy/cos(u[AY_6]))/t->torad;
    u[AY_6]/=t->torad;
}

static void LocalUserToPivotPointVector(struct table *t, double *u,
    double *pp0, double *pp1, double *pp2)
{
    int i,j,k;
    double *ppo0=t->ppo0, *ppo1=t->ppo1, *ppo2=t->ppo2;
    MakeRotationMatrix(t, u);
    for(i=X,k=X_6;i<=Z;i++,k++) {
        pp0[i]=0; pp1[i]=0; pp2[i]=0;
        for(j=X;j<=Z;j++) {
            pp0[i]+=ppo0[j]*t->a[i][j];
            pp1[i]+=ppo1[j]*t->a[i][j];
            pp2[i]+=ppo2[j]*t->a[i][j];
        }
        pp0[i]+=u[k]; pp1[i]+=u[k]; pp2[i]+=u[k];
    }
}

static void PivotPointVectorToMotor(struct table *t,
    double *pp0, double *pp1, double *pp2, double *m, double *u)
{
    double *ppo0=t->ppo0, *ppo1=t->ppo1, *ppo2=t->ppo2;
    double norm[3];
    switch(t->geom) {
    case GEOM_SRI: case GEOM_GEOCARS: case GEOM_PNC: default:
        m[M0X]=pp0[X]-ppo0[X]; m[M0Y]=pp0[Y]-ppo0[Y];
        m[M1Y]=pp1[Y]-ppo1[Y];
        m[M2X]=pp2[X]-ppo2[X]; m[M2Y]=pp2[Y]-ppo2[Y];
        if(t->can_rw_drive[M2Z]) m[M2Z]=pp2[Z]-ppo2[Z];
        else { u[Z_6]=-(t->a[Z][X]*ppo2[X]+t->a[Z][Y]*ppo2[Y]+(t->a[Z][Z]-1)*ppo2[Z]); m[M2Z]=0; }
        break;
    case GEOM_NEWPORT:
        norm[X]=t->a[X][Y]; norm[Y]=t->a[Y][Y]; norm[Z]=t->a[Z][Y];
        m[M0Y]=(pp0[Y]-ppo0[Y])/norm[Y]; m[M1Y]=(pp1[Y]-ppo1[Y])/norm[Y]; m[M2Y]=(pp2[Y]-ppo2[Y])/norm[Y];
        m[M2Z]=(pp2[Z]-ppo2[Z])-norm[Z]*m[M2Y];
        if(t->can_rw_drive[M2X]) {
            m[M0X]=(pp0[X]-ppo0[X])-norm[X]*m[M0Y];
            m[M2X]=(pp2[X]-ppo2[X])-norm[X]*m[M2Y];
        } else {
            u[X_6]=-((t->a[X][X]-1)*ppo2[X]+t->a[X][Y]*ppo2[Y]+t->a[X][Z]*ppo2[Z]-norm[X]*m[M2Y]);
            m[M0X]=(t->a[X][X]-1)*ppo0[X]+t->a[X][Y]*ppo0[Y]+t->a[X][Z]*ppo0[Z]-norm[X]*m[M0Y]+u[X_6];
            m[M2X]=0;
        }
        break;
    }
}

static void MotorToUser(struct table *t, double *m, double *u)
{
    int i,j,k;
    double q0[3],q1[3],q2[3];
    double pp0[3],pp1[3],pp2[3], m_try[6];

    switch(t->geom) {
    case GEOM_SRI: case GEOM_GEOCARS: case GEOM_PNC: default:
        MotorToPivotPointVector(t,m,q0,q1,q2);
        PivotPointVectorToLocalUserAngles(t,q0,q1,q2,u);
        break;
    case GEOM_NEWPORT:
        MotorToLocalUserAngles(t,m,u);
        break;
    }

    MakeRotationMatrix(t, u);
    for(j=X;j<=Z;j++) {
        pp0[j]=pp1[j]=pp2[j]=0;
        for(k=X;k<=Z;k++) {
            pp0[j]+=t->ppo0[k]*t->a[j][k];
            pp1[j]+=t->ppo1[k]*t->a[j][k];
            pp2[j]+=t->ppo2[k]*t->a[j][k];
        }
    }
    if(t->geom==GEOM_NEWPORT) { pp0[Y]+=u[Y_6]; pp1[Y]+=u[Y_6]; pp2[Y]+=u[Y_6]; }
    PivotPointVectorToMotor(t,pp0,pp1,pp2,m_try,u);

    if((t->geom==GEOM_NEWPORT)&&!(t->can_rw_drive[M2X])) ; else u[X_6]=m[M2X]-m_try[M2X];
    if(t->geom!=GEOM_NEWPORT) u[Y_6]=m[M2Y]-m_try[M2Y];
    if(!(t->can_rw_drive[M2Z])) ; else u[Z_6]=m[M2Z]-m_try[M2Z];

    LocalToLab(t, u, u);
    for(i=0;i<6;i++) u[i]-=t->ax0[i];
}

static void UserToMotor(struct table *t, double *user, double *m)
{
    double u[6]; int i;
    for(i=0;i<6;i++) u[i]=user[i]+t->ax0[i];
    LabToLocal(t, u, u);
    LocalUserToPivotPointVector(t, u, t->pp0, t->pp1, t->pp2);
    PivotPointVectorToMotor(t, t->pp0, t->pp1, t->pp2, m, u);
}

/* ---- Test harness ---- */

static void init_table(struct table *t, int geom)
{
    memset(t, 0, sizeof(struct table));
    t->geom = geom;
    t->lx = 200.0; t->lz = 300.0;
    t->sx = 100.0; t->sy = 50.0; t->sz = 150.0;
    t->rx = 0; t->ry = 0; t->rz = 0;
    t->yang = 0;
    t->torad = atan(1.0)/45.0;
    for(int i=0;i<6;i++) { t->ax0[i]=0; t->can_rw_drive[i]=1; }
    D2R = t->torad;
    InitGeometry(t);
}

static void print_motors(const char *label, double *m)
{
    printf("%s: [%.15e, %.15e, %.15e, %.15e, %.15e, %.15e]\n",
        label, m[0],m[1],m[2],m[3],m[4],m[5]);
}

static void print_user(const char *label, double *u)
{
    printf("%s: [%.15e, %.15e, %.15e, %.15e, %.15e, %.15e]\n",
        label, u[0],u[1],u[2],u[3],u[4],u[5]);
}

int main()
{
    struct table t;
    double m[6], u[6], u_back[6];
    int geom;
    const char *geom_names[] = {"SRI", "GEOCARS", "NEWPORT", "PNC"};

    printf("// Golden test values generated from original C tableRecord.c code\n");
    printf("// Geometry: lx=200, lz=300, sx=100, sy=50, sz=150, yang=0\n\n");

    /* Test cases: UserToMotor for each geometry */
    double test_users[][6] = {
        {0, 0, 0, 0, 0, 0},           /* zero */
        {2, 0, 0, 0, 0, 0},           /* pure AX = 2° */
        {0, 1.5, 0, 0, 0, 0},         /* pure AY = 1.5° */
        {0, 0, -1, 0, 0, 0},          /* pure AZ = -1° */
        {0, 0, 0, 3, 0, 0},           /* pure X = 3 */
        {0, 0, 0, 0, 5, 0},           /* pure Y = 5 */
        {0, 0, 0, 0, 0, 7},           /* pure Z = 7 */
        {1, -0.5, 0.3, 2, 1, -1},     /* combined */
        {3, 2, -1, -2, 3, 1.5},       /* another combined */
    };
    int n_tests = sizeof(test_users) / sizeof(test_users[0]);

    for (geom = 0; geom <= 3; geom++) {
        int g = geom;
        if (geom == 2) g = GEOM_NEWPORT;
        if (geom == 3) g = GEOM_PNC;
        init_table(&t, g);
        printf("// === %s ===\n", geom_names[geom]);

        for (int i = 0; i < n_tests; i++) {
            memcpy(u, test_users[i], sizeof(u));
            UserToMotor(&t, u, m);
            printf("// user_to_motor #%d\n", i);
            print_user("  user", u);
            print_motors("  motor", m);

            /* Round-trip: MotorToUser */
            MotorToUser(&t, m, u_back);
            print_user("  roundtrip", u_back);
            printf("\n");
        }
    }

    /* Test with YANG=30° */
    printf("// === SRI with YANG=30 ===\n");
    init_table(&t, GEOM_SRI);
    t.yang = 30.0;
    for (int i = 0; i < n_tests; i++) {
        memcpy(u, test_users[i], sizeof(u));
        UserToMotor(&t, u, m);
        printf("// user_to_motor #%d (yang=30)\n", i);
        print_user("  user", u);
        print_motors("  motor", m);
        MotorToUser(&t, m, u_back);
        print_user("  roundtrip", u_back);
        printf("\n");
    }

    /* Test with offsets */
    printf("// === SRI with offset ax0=[5,0,0,0,0,0] ===\n");
    init_table(&t, GEOM_SRI);
    t.ax0[0] = 5.0;  /* 5° offset on AX */
    double offset_user[] = {0, 0, 0, 0, 0, 0};  /* "zero" with offset = 5° real */
    UserToMotor(&t, offset_user, m);
    printf("// user=[0,0,0,0,0,0] with ax0=[5,0,0,0,0,0]\n");
    print_motors("  motor", m);
    MotorToUser(&t, m, u_back);
    print_user("  roundtrip", u_back);

    return 0;
}
