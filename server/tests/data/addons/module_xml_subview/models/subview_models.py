from odoo import fields, models


class SubviewParent(models.Model):
    _name = "module_xml_subview.parent"
    _description = "Subview Parent"

    amount = fields.Float()
    line_ids = fields.One2many("module_xml_subview.line", "parent_id")
    tag_id = fields.Many2one(comodel_name="module_xml_subview.tag")
    ref_id = fields.Reference(selection=[("module_xml_subview.line", "Line")])


class SubviewLine(models.Model):
    _name = "module_xml_subview.line"
    _description = "Subview Line"

    amount = fields.Float()
    parent_id = fields.Many2one("module_xml_subview.parent")


class SubviewTag(models.Model):
    _name = "module_xml_subview.tag"
    _description = "Subview Tag"

    amount = fields.Float()
