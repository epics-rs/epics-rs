/*
 * Oracle for reference_snc_parse: C snc's own main(), lexer and parser
 * (objects from the sequencer build, main renamed snc_main), with
 * analyse_program replaced by a dump of the syntax tree it receives.
 * The Rust side prints the same format.
 */
#include <stdio.h>

#include "types.h"
#include "var_types.h"

int snc_main(int argc, char *argv[]);

static const char *nz(const char *s)
{
    return s ? s : "(null)";
}

static Node *pending[64];
static int npending;

static void show_type(Type *t)
{
    if (t->is_const)
        printf("const ");
    switch (t->tag) {
    case T_NONE: printf("none"); break;
    case T_EVFLAG: printf("evflag"); break;
    case T_VOID: printf("void"); break;
    case T_PRIM: printf("prim:%d", (int)t->val.prim); break;
    case T_FOREIGN:
        printf("foreign:%d:%s", (int)t->val.foreign.tag, nz(t->val.foreign.name));
        break;
    case T_POINTER:
        printf("ptr(");
        show_type(t->val.pointer.value_type);
        printf(")");
        break;
    case T_ARRAY:
        printf("arr%u(", t->val.array.num_elems);
        show_type(t->val.array.elem_type);
        printf(")");
        break;
    case T_FUNCTION:
        printf("fn(");
        show_type(t->val.function.return_type);
        printf(")");
        pending[npending++] = t->val.function.param_decls;
        break;
    case T_STRUCT: printf("struct:%s", nz(t->val.structure.name)); break;
    }
}

static void dump_list(Node *l, int level);

static void dump_node(Node *e, int level)
{
    int i, first, n;
    Node *params[64];

    printf("%*s%s sym=%d str=%s file=%s line=%d", level * 2, "",
        node_info[e->tag].name, e->token.symbol, nz(e->token.str),
        nz(e->token.file), e->token.line);
    if (!e->last) {
        printf(" last=-");
    } else if (e->last == e) {
        printf(" last=self");
    } else {
        Node *p = e;
        int k = 0;
        while (p && p != e->last) {
            p = p->next;
            k++;
        }
        if (p)
            printf(" last=+%d", k);
        else
            printf(" last=?");
    }
    first = npending;
    if (e->tag == D_DECL) {
        Var *v = e->extra.e_decl;
        printf(" var=%s decl=%s type=", nz(v->name), v->decl == e ? "self" : "other");
        show_type(v->type);
    }
    if (e->tag == D_OPTION)
        printf(" value=%u", e->extra.e_option);
    printf("\n");
    n = npending - first;
    for (i = 0; i < n; i++)
        params[i] = pending[first + i];
    npending = first;
    for (i = 0; i < n; i++) {
        printf("%*sparams\n", level * 2 + 2, "");
        dump_list(params[i], level + 2);
    }
    for (i = 0; i < node_info[e->tag].num_children; i++) {
        if (e->children[i]) {
            printf("%*s[%d]\n", level * 2 + 2, "", i);
            dump_list(e->children[i], level + 2);
        }
    }
}

static void dump_list(Node *l, int level)
{
    Node *e;
    for (e = l; e; e = e->next)
        dump_node(e, level);
}

Program *analyse_program(Node *ep, Options options)
{
    (void)options;
    dump_list(ep, 0);
    fflush(stdout);
    return 0;
}

void generate_code(Program *p)
{
    (void)p;
}

int main(int argc, char *argv[])
{
    return snc_main(argc, argv);
}
