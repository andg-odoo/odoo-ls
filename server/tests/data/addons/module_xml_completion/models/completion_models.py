from odoo import fields, models


class CompletionParent(models.Model):
    _name = "module_xml_completion.parent"
    _description = "Completion Parent"

    amount = fields.Float()
    total = fields.Float()
    line_ids = fields.One2many("module_xml_completion.line", "parent_id")
    group_id = fields.Many2one("res.groups")
    partner_id = fields.Many2one("res.partner")
    owner_id = fields.Many2one("res.partner")
    default_amount = fields.Float(default=lambda self: 0.0)

    def action_completion_confirm(self):
        pass

    def _completion_private(self):
        pass

    def __completion_dunder__(self):
        pass


class CompletionLine(models.Model):
    _name = "module_xml_completion.line"
    _description = "Completion Line"

    line_amount = fields.Float()
    parent_id = fields.Many2one("module_xml_completion.parent")
